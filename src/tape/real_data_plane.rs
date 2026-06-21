//! [`RealTapeDataPlane`] — the **production** [`TapeDataPlane`] implementation
//! (Phase 3, the "hydration bridge").
//!
//! Where [`MockTapeDataPlane`](crate::tape::data_plane::MockTapeDataPlane) is an
//! in-memory test backing store, this is the real one: it wires the P5 paging
//! **control plane** to the P0-P2 [`context_tape`] **data plane** plus pgmcp's
//! read-only corpus. The seam methods do exactly what the
//! [`crate::tape::data_plane`] contract specifies:
//!
//! - **`get` / `get_many`** — resolve `PageAddr → PageAddress`, consult the
//!   per-tree [`context_tape::TapeStore`] (hot tier → out-of-core overlay via
//!   [`get_cascade`](context_tape::TapeStore::get_cascade)); on a miss, hydrate
//!   the durable row ([`crate::tape::hydrate`]), admit it **clean**
//!   ([`insert_hydrated`](context_tape::TapeStore::insert_hydrated)), and return
//!   its situated bytes.
//! - **`put`** — write the bytes into the per-tree store **dirty**
//!   ([`put`](context_tape::TapeStore::put)). The write *back* to durable storage
//!   is a bi-temporal supersession into `memory_observations` (NEVER
//!   `file_chunks` — the corpus *file* tables `file_chunks` / `indexed_files` are
//!   read-only; only `memory_observations` is writable, and only via this gated
//!   path), and is **gated off by default** (`[tape] allow_promotion = false`):
//!   with promotion off, the bytes live only in the store and are discarded on
//!   eviction.
//! - **`resolve`** — turn a [`PageQuery`] into [`PageRef`]s **without hydrating
//!   bytes**: chunk-range / semantic-k (this is the embedding path P5 deferred —
//!   it embeds via [`crate::embed::EmbedSource`] then runs
//!   [`crate::db::queries::semantic_search`]) / grep.
//! - **`summary_of`** — locate the covering summary node
//!   (`memory_summary_tree` / `code_summary_tree`) standing in for a leaf set.
//!
//! ## Trust boundary
//!
//! Residency is the controller's decision; this plane only *moves bytes*. It
//! reads the corpus *file* tables (`file_chunks` / `indexed_files`) exclusively
//! through [`crate::tape::hydrate`] and treats them as read-only;
//! `memory_observations` is written only by the gated, default-off promotion path
//! ([`supersede_observation`](RealTapeDataPlane::supersede_observation), behind
//! `[tape] allow_promotion`, bi-temporal). It never runs a shell and never writes
//! the user's files. ADR-021: a DB-IO fault in hydrate / put is mapped to
//! [`TapeError::Backend`] (the controller logs it `error!`); an address that
//! resolves to no row is [`TapeError::NotFound`].

use std::collections::HashMap;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tracing::{error, warn};

use context_tape::{Page, PageAddress, PageKind as TapePageKind, TreeId};

use crate::embed::EmbedSource;
use crate::tape::address_resolve::{address_to_pageaddr, pageaddr_to_address};
use crate::tape::data_plane::{
    PageContent, PageQuery, PageRef, TapeDataPlane, TapeError, TreePath,
};
use crate::tape::hydrate::{self, HydrateError};
use crate::tape::registry::TapeRegistry;
use crate::tape::vocab::PageKind;

/// Default top-k cap for a `resolve(Grep)` / `resolve(Chunk)` candidate scan, so
/// a pathological pattern cannot return an unbounded ref list to the controller.
const RESOLVE_SCAN_LIMIT: i32 = 256;

/// `hnsw.ef_search` used for the `resolve(Semantic)` k-NN. Matches the value the
/// MCP `semantic_search` tool uses for recall/latency balance.
const SEMANTIC_EF_SEARCH: i32 = 100;

/// Map a [`HydrateError`] to a [`TapeError`]: a real DB fault → `Backend`
/// (ADR-021 `error!` at the controller); a benign miss / non-hydratable scratch
/// → `NotFound` (a coverage gap, not a crash).
fn hydrate_err_to_tape(addr_path: &str, err: HydrateError) -> TapeError {
    match err {
        HydrateError::Db(e) => {
            error!(error = %e, addr = addr_path, "tape hydrate DB read failed");
            TapeError::Backend(format!("hydrate {addr_path}: {e}"))
        }
        HydrateError::NotFound(p) => TapeError::NotFound(p),
        HydrateError::NotHydratable(p) => TapeError::NotFound(p),
    }
}

/// Translate the data-plane page kind into the control-plane [`PageKind`]
/// vocabulary (the two are intentionally parallel closed sets). Currently used
/// only to validate the closed-set parity in tests — the resolve/get paths set
/// the control-plane kind directly from the resolved address shape — so it is
/// gated behind `cfg(test)` until a caller needs the runtime translation.
#[cfg(test)]
fn kind_of(page: &Page) -> PageKind {
    match page.meta.kind {
        TapePageKind::FileChunk => PageKind::FileChunk,
        TapePageKind::MemoryObservation => PageKind::MemoryObservation,
        TapePageKind::SummaryNode => PageKind::SummaryNode,
        // A scratch page surfaced to the controller is treated as a file-chunk-
        // shaped leaf (the controller has no scratch kind; scratch is an
        // implementation detail of the store).
        TapePageKind::Scratch => PageKind::FileChunk,
    }
}

/// The production [`TapeDataPlane`]. Holds the DB pool, the query-time embedding
/// source, and the per-tree store registry. Cheap to construct; borrowed by the
/// [`PagingEngine`](crate::tape::engine::PagingEngine).
pub struct RealTapeDataPlane<'a> {
    pool: &'a PgPool,
    embed: &'a EmbedSource,
    registry: &'a TapeRegistry,
    /// Whether a dirty page's write-back may promote into `memory_observations`
    /// (`[tape] allow_promotion`). Off by default — see the module docs.
    allow_promotion: bool,
}

impl<'a> RealTapeDataPlane<'a> {
    /// Construct over the daemon's pool, embedding source, and tape registry.
    /// `allow_promotion` comes from `[tape] allow_promotion` (default `false`).
    pub fn new(
        pool: &'a PgPool,
        embed: &'a EmbedSource,
        registry: &'a TapeRegistry,
        allow_promotion: bool,
    ) -> Self {
        Self {
            pool,
            embed,
            registry,
            allow_promotion,
        }
    }

    /// Wire a `RealTapeDataPlane` from a [`SystemContext`](crate::context::SystemContext):
    /// the raw `&PgPool` (via the `DbClient` escape hatch), the query-time
    /// [`EmbedSource`], the shared per-tree [`TapeRegistry`], and the
    /// `[tape] allow_promotion` flag. This is the production wiring seam the P5
    /// [`PagingEngine`](crate::tape::engine::PagingEngine) runs over.
    ///
    /// Returns `None` in CLI / mock-DB mode where the `DbClient` is not a live
    /// `PgPool` (the tape's hydrate / supersede paths require real Postgres);
    /// callers in that mode fall back to the in-memory mock data plane.
    pub fn from_context(ctx: &'a crate::context::SystemContext) -> Option<Self> {
        let pool = ctx.db().pool()?;
        let allow_promotion = ctx.config().load().tape.allow_promotion;
        Some(Self::new(
            pool,
            ctx.embed(),
            ctx.tape_registry(),
            allow_promotion,
        ))
    }

    /// Derive the per-tree [`TreeId`] from a [`TreePath`] string deterministically
    /// (SHA-256 of the path, first 16 bytes → `Uuid`). Total over arbitrary tree
    /// paths (including non-UUID test ids), and stable across calls / process
    /// restarts, so a tree's store and its `Scratch` namespace are reconstructed
    /// identically. This is the SOLE authority for `TreePath → TreeId`, so the
    /// derivation need only be deterministic, not invertible.
    ///
    /// Public so a caller (or a test) that needs to address the same per-tree
    /// store in the [`TapeRegistry`] can map a `TreePath` to its `TreeId` without
    /// re-deriving the hash out of band.
    pub fn tree_id(tree: &TreePath) -> TreeId {
        let digest = Sha256::digest(tree.as_str().as_bytes());
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        TreeId::from_bytes(bytes)
    }

    /// Resolve a [`PageAddr`](crate::tape::working_set::PageAddr) (the
    /// control-plane string) into a typed [`PageAddress`], mapping a malformed
    /// string to a benign `NotFound`.
    fn parse_addr(addr_str: &str) -> Result<PageAddress, TapeError> {
        pageaddr_to_address(&crate::tape::working_set::PageAddr(addr_str.to_string()))
            .ok_or_else(|| TapeError::NotFound(addr_str.to_string()))
    }

    /// Fetch one page's situated bytes: hot/OOC cascade in the per-tree store,
    /// else hydrate from the corpus and admit it clean. Returns the `(bytes,
    /// est_tokens)` the controller wants.
    async fn fetch_one(
        &self,
        tree: &TreePath,
        address: &PageAddress,
    ) -> Result<(String, i32), TapeError> {
        let tree_id = Self::tree_id(tree);

        // 1. Resident (hot or out-of-core overlay)?
        if let Some(page) = self
            .registry
            .with_store(tree_id, |s| s.get_cascade(address))
        {
            return Ok((page.content.clone(), page.meta.est_tokens as i32));
        }

        // 2. Cold: a Scratch page that is not resident has no corpus backing — it
        //    is genuinely gone (NotFound). Only corpus addresses hydrate.
        if matches!(address, PageAddress::Scratch { .. }) {
            return Err(TapeError::NotFound(address.to_path()));
        }

        // 3. Hydrate from the durable corpus (READ-ONLY).
        let page = hydrate::hydrate(self.pool, address)
            .await
            .map_err(|e| hydrate_err_to_tape(&address.to_path(), e))?;
        let bytes = page.content.clone();
        let est_tokens = page.meta.est_tokens as i32;

        // 4. Admit the freshly-hydrated page into the hot tier as CLEAN.
        self.registry.with_store_mut(tree_id, |s| {
            s.insert_hydrated(address.clone(), page);
        });

        Ok((bytes, est_tokens))
    }
}

#[async_trait]
impl TapeDataPlane for RealTapeDataPlane<'_> {
    async fn get(
        &self,
        tree: &TreePath,
        addr: &crate::tape::working_set::PageAddr,
    ) -> Result<PageContent, TapeError> {
        let address = Self::parse_addr(addr.as_str())?;
        let (bytes, est_tokens) = self.fetch_one(tree, &address).await?;
        Ok(PageContent {
            addr: addr.clone(),
            bytes,
            est_tokens,
        })
    }

    async fn get_many(
        &self,
        tree: &TreePath,
        addrs: &[crate::tape::working_set::PageAddr],
    ) -> Result<Vec<PageContent>, TapeError> {
        let mut out = Vec::with_capacity(addrs.len());
        for addr in addrs {
            let address = Self::parse_addr(addr.as_str())?;
            let (bytes, est_tokens) = self.fetch_one(tree, &address).await?;
            out.push(PageContent {
                addr: addr.clone(),
                bytes,
                est_tokens,
            });
        }
        Ok(out)
    }

    async fn put(
        &self,
        tree: &TreePath,
        addr: &crate::tape::working_set::PageAddr,
        bytes: &str,
    ) -> Result<(), TapeError> {
        let address = Self::parse_addr(addr.as_str())?;
        let tree_id = Self::tree_id(tree);

        // Stage the write into the per-tree store as DIRTY. If `bytes` is empty
        // (the engine's "flush staged dirty content" signal — the control plane
        // does not carry page bytes today), and the page is already resident, we
        // re-mark it dirty in place rather than clobbering its content with "".
        self.registry.with_store_mut(tree_id, |s| {
            // Empty `bytes` is the engine's "flush staged dirty content" signal:
            // re-stage the page's *existing* content as dirty (no content change)
            // rather than clobbering it with "".
            let existing_to_restage = if bytes.is_empty() {
                s.get(&address).cloned()
            } else {
                None
            };
            if let Some(existing) = existing_to_restage {
                s.put(address.clone(), existing);
                return;
            }
            let est_tokens = Page::estimate_tokens(bytes);
            // A written-through page inherits a neutral importance; the controller
            // tracks the authoritative importance on its resident-page row.
            let page = Page::new(
                address.clone(),
                bytes.to_string(),
                context_tape::PageMeta {
                    kind: TapePageKind::Scratch,
                    est_tokens,
                    importance: 0.5,
                    dirty: true,
                },
            );
            s.put(address.clone(), page);
        });

        // Write-BACK (promotion) into durable memory — gated OFF by default.
        if !self.allow_promotion {
            // By-design no-op: a stray write never leaks into durable memory.
            // (ADR-021 warn!: a documented, by-design refusal, not an error.)
            warn!(
                addr = addr.as_str(),
                "context-tape: write-back promotion is disabled ([tape] allow_promotion=false); \
                 dirty bytes staged in the tree store only"
            );
            return Ok(());
        }

        // Promotion ON: supersede into `memory_observations` for a corpus page
        // that maps to an existing observation; otherwise the write has no
        // durable target (only observations are writable — chunks/files are
        // read-only) and is staged-only.
        if let PageAddress::Observation { obs_id } = address {
            self.supersede_observation(obs_id, bytes).await?;
        } else {
            warn!(
                addr = addr.as_str(),
                "context-tape: write-back has no durable observation target for this address kind; \
                 staged in the tree store only (corpus is read-only)"
            );
        }
        Ok(())
    }

    async fn resolve(&self, tree: &TreePath, query: &PageQuery) -> Result<Vec<PageRef>, TapeError> {
        match query {
            PageQuery::Chunk { path, lo, hi } => self.resolve_chunk_range(path, *lo, *hi).await,
            PageQuery::Semantic { query, k } => self.resolve_semantic(query, *k).await,
            PageQuery::Grep { pattern } => self.resolve_grep(pattern).await,
        }
        .map_err(|e| {
            // resolve faults are DB/embedding-side; map and let the caller log.
            match e {
                ResolveError::Db(err) => {
                    error!(error = %err, tree = tree.as_str(), "tape resolve DB read failed");
                    TapeError::Backend(format!("resolve: {err}"))
                }
                ResolveError::Embed(msg) => {
                    error!(error = %msg, tree = tree.as_str(), "tape resolve embedding failed");
                    TapeError::Backend(format!("resolve embed: {msg}"))
                }
            }
        })
    }

    async fn summary_of(
        &self,
        _tree: &TreePath,
        leaf_addrs: &[crate::tape::working_set::PageAddr],
    ) -> Result<Option<PageRef>, TapeError> {
        // Locate the covering summary node for the leaf set. We support the two
        // leaf kinds that have a summary tree:
        //   - memory observations  → memory_summary_tree (parent of the obs node)
        //   - corpus file chunks    → code_summary_tree   (a cluster covering the file)
        // The first leaf that resolves to a covering summary wins; if none do,
        // return None (the demotion ladder then logs a by-design warn! and keeps
        // the leaves evicted without a stand-in).
        for leaf in leaf_addrs {
            let Some(address) = pageaddr_to_address(leaf) else {
                continue;
            };
            match address {
                PageAddress::Observation { obs_id } => {
                    match self.observation_summary(obs_id).await {
                        Ok(Some(r)) => return Ok(Some(r)),
                        Ok(None) => continue,
                        Err(e) => {
                            error!(error = %e, "tape summary_of (memory) DB read failed");
                            return Err(TapeError::Backend(format!("summary_of: {e}")));
                        }
                    }
                }
                PageAddress::Chunk { chunk_id } => match self.chunk_summary(chunk_id).await {
                    Ok(Some(r)) => return Ok(Some(r)),
                    Ok(None) => continue,
                    Err(e) => {
                        error!(error = %e, "tape summary_of (code) DB read failed");
                        return Err(TapeError::Backend(format!("summary_of: {e}")));
                    }
                },
                _ => continue,
            }
        }
        Ok(None)
    }
}

/// Internal error for the resolve family (kept private; mapped to `TapeError` at
/// the trait boundary so the variant carries the right ADR-021 log class).
enum ResolveError {
    Db(sqlx::Error),
    Embed(String),
}
impl From<sqlx::Error> for ResolveError {
    fn from(e: sqlx::Error) -> Self {
        ResolveError::Db(e)
    }
}

impl RealTapeDataPlane<'_> {
    /// `resolve(Chunk{path,lo,hi})` — metadata-only refs for a chunk-index span
    /// of one file. Reads `file_chunks` (joined to get the file's path/id) via the
    /// canonical [`crate::db::queries::get_chunks_in_index_range`] for the
    /// span's rows, then maps each chunk to its `Chunk` address ref WITHOUT
    /// hydrating its situated bytes (only id + line span + a token estimate of the
    /// RAW chunk text, which is already in hand from the range read).
    async fn resolve_chunk_range(
        &self,
        path: &str,
        lo: i32,
        hi: i32,
    ) -> Result<Vec<PageRef>, ResolveError> {
        // The range reader returns raw chunk rows (no ids); pull ids + importance
        // proxy with a parallel id query bounded to the same span.
        let rows: Vec<(i64, String, i64)> = sqlx::query_as(
            "SELECT c.id, c.content, COALESCE(imp.cnt, 0) AS importer_count
             FROM file_chunks c
             JOIN indexed_files f ON f.id = c.file_id
             LEFT JOIN LATERAL (
                 SELECT COUNT(DISTINCT e.source_file_id) AS cnt
                 FROM code_graph_edges e
                 WHERE e.target_file_id = c.file_id AND e.edge_type = 'import'
             ) imp ON true
             WHERE (f.path = $1 OR f.relative_path = $1)
               AND c.chunk_index >= $2
               AND c.chunk_index <= $3
             ORDER BY c.chunk_index
             LIMIT $4",
        )
        .bind(path)
        .bind(lo)
        .bind(hi)
        .bind(RESOLVE_SCAN_LIMIT)
        .fetch_all(self.pool)
        .await?;

        let mut refs = Vec::with_capacity(rows.len());
        for (chunk_id, content, importer_count) in rows {
            let address = PageAddress::Chunk { chunk_id };
            refs.push(PageRef {
                addr: address_to_pageaddr(&address),
                kind: PageKind::FileChunk,
                // A metadata token estimate over the RAW chunk (the situating
                // prefix is added at hydrate time; this is a budgeting estimate).
                est_tokens: Page::estimate_tokens(&content) as i32,
                importance: hydrate::importance_from_importers_pub(importer_count),
            });
        }
        Ok(refs)
    }

    /// `resolve(Semantic{query,k})` — the embedding path P5 deferred. Embeds the
    /// natural-language query via [`EmbedSource::embed_query`], runs
    /// [`crate::db::queries::semantic_search`], and maps each hit to a
    /// `Chunk` address ref. Metadata-only: no situated bytes are produced (the
    /// hit's `chunk_content` gives a token estimate; the chunk id is resolved from
    /// `(path, chunk content)` so the ref addresses a stable `Chunk`).
    async fn resolve_semantic(&self, query: &str, k: usize) -> Result<Vec<PageRef>, ResolveError> {
        let embedding = self
            .embed
            .embed_query(query)
            .await
            .map_err(|e| ResolveError::Embed(e.to_string()))?;
        let hits = crate::db::queries::semantic_search(
            self.pool,
            &embedding,
            k as i32,
            None,
            None,
            SEMANTIC_EF_SEARCH,
            false,
        )
        .await?;

        // `semantic_search` does not select chunk_id; resolve it from the file
        // path + the hit's line span (stable for a chunk). Batch the lookup into a
        // SINGLE query keyed by `(path, start_line, end_line)` rather than firing
        // one `SELECT` per hit (the old N+1, up to `k` round-trips). The join
        // disjunction `(f.path = k.path OR f.relative_path = k.path)` reproduces
        // the per-hit `(f.path = $1 OR f.relative_path = $1)` exactly, and we
        // select `k.path` back so the result keys by the SAME value the hit
        // carries regardless of which column matched.
        let chunk_ids = self.chunk_ids_by_line_span(&hits).await?;

        let mut refs = Vec::with_capacity(hits.len());
        for hit in hits {
            let Some(&chunk_id) = chunk_ids.get(&(hit.path.clone(), hit.start_line, hit.end_line))
            else {
                continue; // hit no longer maps to a live chunk row
            };
            let address = PageAddress::Chunk { chunk_id };
            // The semantic score (0..1) is the importance signal for ranking.
            let importance = hit.score.unwrap_or(0.0).clamp(0.0, 1.0) as f32;
            refs.push(PageRef {
                addr: address_to_pageaddr(&address),
                kind: PageKind::FileChunk,
                est_tokens: Page::estimate_tokens(&hit.chunk_content) as i32,
                importance,
            });
        }
        Ok(refs)
    }

    /// `resolve(Grep{pattern})` — metadata-only refs for chunks whose content
    /// matches `pattern`, via [`crate::db::queries::grep_search_chunks`].
    async fn resolve_grep(&self, pattern: &str) -> Result<Vec<PageRef>, ResolveError> {
        let hits = crate::db::queries::grep_search_chunks(
            self.pool,
            pattern,
            None,
            None,
            None,
            false,
            RESOLVE_SCAN_LIMIT,
            false,
        )
        .await?;
        // Resolve the chunk id from path + chunk_index (both in the hit). Batch
        // the lookup into a SINGLE query keyed by `(path, chunk_index)` instead of
        // one `SELECT` per hit (the old N+1, up to `RESOLVE_SCAN_LIMIT`
        // round-trips). The join disjunction reproduces the per-hit `(f.path = $1
        // OR f.relative_path = $1)` exactly; `k.path` is selected back so the
        // result keys by the value the hit carries regardless of which column
        // matched.
        let chunk_ids = self.chunk_ids_by_chunk_index(&hits).await?;

        let mut refs = Vec::with_capacity(hits.len());
        for hit in hits {
            let Some(&chunk_id) = chunk_ids.get(&(hit.path.clone(), hit.chunk_index)) else {
                continue;
            };
            let address = PageAddress::Chunk { chunk_id };
            refs.push(PageRef {
                addr: address_to_pageaddr(&address),
                kind: PageKind::FileChunk,
                est_tokens: Page::estimate_tokens(&hit.content) as i32,
                // Grep is a boolean filter, not a ranker: neutral importance.
                importance: 0.5,
            });
        }
        Ok(refs)
    }

    /// Batch-resolve `chunk_id` for a set of `(path, start_line, end_line)` keys
    /// (the `resolve_semantic` recovery), collapsing what was an N+1 of one
    /// `SELECT c.id … LIMIT 1` per hit into a SINGLE round-trip. The `UNNEST`'d
    /// key relation is joined to `file_chunks`/`indexed_files` with the SAME
    /// `(f.path = k.path OR f.relative_path = k.path)` disjunction the per-hit
    /// query used, and `k.path` is selected back so the returned map keys by the
    /// hit's own `path` value irrespective of which column matched. `DISTINCT ON`
    /// keeps the first id per key (deterministic where the old per-hit `LIMIT 1`
    /// was arbitrary), and a key with no live chunk is simply absent from the map —
    /// the caller then takes its existing "not found → skip" path.
    async fn chunk_ids_by_line_span(
        &self,
        hits: &[crate::db::queries::SearchResult],
    ) -> Result<HashMap<(String, i32, i32), i64>, ResolveError> {
        if hits.is_empty() {
            return Ok(HashMap::new());
        }
        let mut paths = Vec::with_capacity(hits.len());
        let mut start_lines = Vec::with_capacity(hits.len());
        let mut end_lines = Vec::with_capacity(hits.len());
        for hit in hits {
            paths.push(hit.path.clone());
            start_lines.push(hit.start_line);
            end_lines.push(hit.end_line);
        }

        let rows: Vec<(String, i32, i32, i64)> = sqlx::query_as(
            "SELECT DISTINCT ON (k.path, k.start_line, k.end_line)
                    k.path, k.start_line, k.end_line, c.id
             FROM UNNEST($1::text[], $2::int4[], $3::int4[])
                      AS k(path, start_line, end_line)
             JOIN indexed_files f
               ON (f.path = k.path OR f.relative_path = k.path)
             JOIN file_chunks c
               ON c.file_id = f.id
              AND c.start_line = k.start_line
              AND c.end_line = k.end_line
             ORDER BY k.path, k.start_line, k.end_line, c.id",
        )
        .bind(&paths)
        .bind(&start_lines)
        .bind(&end_lines)
        .fetch_all(self.pool)
        .await?;

        let mut map = HashMap::with_capacity(hits.len());
        for (path, start_line, end_line, chunk_id) in rows {
            map.entry((path, start_line, end_line)).or_insert(chunk_id);
        }
        Ok(map)
    }

    /// Batch-resolve `chunk_id` for a set of `(path, chunk_index)` keys (the
    /// `resolve_grep` recovery), collapsing the per-hit N+1 into a SINGLE
    /// round-trip. Mirrors [`chunk_ids_by_line_span`](Self::chunk_ids_by_line_span):
    /// same `(f.path = k.path OR f.relative_path = k.path)` disjunction,
    /// `k.path` selected back to key by the hit's own value, `DISTINCT ON` for a
    /// deterministic first-id-per-key, absent key ⇒ caller's existing skip path.
    async fn chunk_ids_by_chunk_index(
        &self,
        hits: &[crate::db::queries::GrepChunkResult],
    ) -> Result<HashMap<(String, i32), i64>, ResolveError> {
        if hits.is_empty() {
            return Ok(HashMap::new());
        }
        let mut paths = Vec::with_capacity(hits.len());
        let mut chunk_indexes = Vec::with_capacity(hits.len());
        for hit in hits {
            paths.push(hit.path.clone());
            chunk_indexes.push(hit.chunk_index);
        }

        let rows: Vec<(String, i32, i64)> = sqlx::query_as(
            "SELECT DISTINCT ON (k.path, k.chunk_index)
                    k.path, k.chunk_index, c.id
             FROM UNNEST($1::text[], $2::int4[])
                      AS k(path, chunk_index)
             JOIN indexed_files f
               ON (f.path = k.path OR f.relative_path = k.path)
             JOIN file_chunks c
               ON c.file_id = f.id
              AND c.chunk_index = k.chunk_index
             ORDER BY k.path, k.chunk_index, c.id",
        )
        .bind(&paths)
        .bind(&chunk_indexes)
        .fetch_all(self.pool)
        .await?;

        let mut map = HashMap::with_capacity(hits.len());
        for (path, chunk_index, chunk_id) in rows {
            map.entry((path, chunk_index)).or_insert(chunk_id);
        }
        Ok(map)
    }

    /// Supersede a memory observation bi-temporally: close the prior version's
    /// validity (`valid_to = NOW()`) and insert a fresh `valid_from` row with the
    /// new content — NEVER an in-place mutation, so older trace positions still
    /// read the older bytes. Writes ONLY `memory_observations`. Gated by the
    /// caller on `allow_promotion`.
    async fn supersede_observation(&self, obs_id: i64, bytes: &str) -> Result<(), TapeError> {
        let mut tx = self.pool.begin().await.map_err(|e| {
            error!(error = %e, obs_id, "tape put: begin tx failed");
            TapeError::Backend(format!("put begin: {e}"))
        })?;

        // Read the prior row's identity (entity_id, source) so the new version is
        // a faithful continuation; if it is already closed/absent, there is
        // nothing to supersede (benign).
        let prior: Option<(i64, String)> = sqlx::query_as(
            "SELECT entity_id, source::text
             FROM memory_observations
             WHERE id = $1 AND valid_to IS NULL",
        )
        .bind(obs_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| {
            error!(error = %e, obs_id, "tape put: read prior observation failed");
            TapeError::Backend(format!("put read: {e}"))
        })?;
        let Some((entity_id, source)) = prior else {
            // Nothing active to supersede — by-design benign (warn!, not error).
            warn!(
                obs_id,
                "context-tape: write-back found no active observation to supersede"
            );
            tx.commit().await.ok();
            return Ok(());
        };

        // Close the prior version.
        sqlx::query("UPDATE memory_observations SET valid_to = NOW() WHERE id = $1")
            .bind(obs_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                error!(error = %e, obs_id, "tape put: close prior observation failed");
                TapeError::Backend(format!("put close: {e}"))
            })?;

        // Insert the fresh version (new content; same entity + source; derived
        // from the prior). content_sha256 is required NOT NULL.
        let sha = format!("{:x}", Sha256::digest(bytes.as_bytes()));
        sqlx::query(
            "INSERT INTO memory_observations
                (entity_id, content, content_sha256, importance, source, derived_from, valid_from)
             VALUES ($1, $2, $3, 0.5, $4::memory_source, ARRAY[$5]::bigint[], NOW())",
        )
        .bind(entity_id)
        .bind(bytes)
        .bind(&sha)
        .bind(&source)
        .bind(obs_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            error!(error = %e, obs_id, "tape put: insert superseding observation failed");
            TapeError::Backend(format!("put insert: {e}"))
        })?;

        tx.commit().await.map_err(|e| {
            error!(error = %e, obs_id, "tape put: commit failed");
            TapeError::Backend(format!("put commit: {e}"))
        })?;
        Ok(())
    }

    /// Locate the covering `memory_summary_tree` node for an observation: the
    /// level-0 node points at the observation, and its parent (level ≥ 1) carries
    /// the `summary_text`. Returns a `SummaryNode` ref (addressed by the parent's
    /// backing observation, if any — else `None`). Metadata-only.
    async fn observation_summary(&self, obs_id: i64) -> Result<Option<PageRef>, sqlx::Error> {
        let row: Option<(Option<String>, Option<i64>)> = sqlx::query_as(
            "SELECT parent.summary_text, parent.observation_id
             FROM memory_summary_tree leaf
             JOIN memory_summary_tree parent ON parent.id = leaf.parent_id
             WHERE leaf.observation_id = $1
             LIMIT 1",
        )
        .bind(obs_id)
        .fetch_optional(self.pool)
        .await?;
        let Some((summary_text, parent_obs)) = row else {
            return Ok(None);
        };
        let Some(text) = summary_text else {
            return Ok(None);
        };
        // Address the summary by its backing observation when present so the
        // controller can later `get` it; if the parent has no backing obs, there
        // is no fetchable address, so treat as no summary.
        let Some(parent_obs_id) = parent_obs else {
            return Ok(None);
        };
        let address = PageAddress::Observation {
            obs_id: parent_obs_id,
        };
        Ok(Some(PageRef {
            addr: address_to_pageaddr(&address),
            kind: PageKind::SummaryNode,
            est_tokens: Page::estimate_tokens(&text) as i32,
            importance: 0.6,
        }))
    }

    /// Locate a covering `code_summary_tree` cluster for a chunk's file: a
    /// cluster whose `member_paths` array contains the chunk's file path. Returns
    /// a `SummaryNode` ref. The summary has no fetchable corpus address (it is a
    /// derived cluster, not a row the data plane can `get`), so a future phase
    /// that wants the demotion ladder to *page in* a code summary would persist it
    /// as an observation first; today this returns the ref so the ladder can
    /// account for it but it will only be admitted if its address is fetchable.
    /// To keep `summary_of`'s contract honest (a ref the ladder may `get`), we
    /// return `None` for code chunks until a fetchable address exists.
    async fn chunk_summary(&self, chunk_id: i64) -> Result<Option<PageRef>, sqlx::Error> {
        // Confirm the chunk exists + has a covering cluster (diagnostic read), but
        // do not synthesize a non-fetchable ref. Returning None keeps the demotion
        // ladder's "no stand-in" path correct rather than pointing it at an
        // address `get` cannot resolve.
        let _covered: Option<i64> = sqlx::query_scalar(
            "SELECT cst.id
             FROM file_chunks c
             JOIN indexed_files f ON f.id = c.file_id
             JOIN code_summary_tree cst
               ON f.relative_path = ANY(cst.member_paths)
             WHERE c.id = $1
             LIMIT 1",
        )
        .bind(chunk_id)
        .fetch_optional(self.pool)
        .await?;
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_id_is_deterministic_and_total() {
        let t1 = TreePath::for_root_task("t-abc");
        let t2 = TreePath::for_root_task("t-abc");
        let t3 = TreePath::for_root_task("t-xyz");
        assert_eq!(
            RealTapeDataPlane::tree_id(&t1),
            RealTapeDataPlane::tree_id(&t2),
            "same tree path → same TreeId (stable across calls)"
        );
        assert_ne!(
            RealTapeDataPlane::tree_id(&t1),
            RealTapeDataPlane::tree_id(&t3),
            "distinct tree paths → distinct TreeIds"
        );
        // Works for a non-UUID id (the property that motivated the hash).
        let _ = RealTapeDataPlane::tree_id(&TreePath::for_root_task("not-a-uuid-at-all"));
    }

    #[test]
    fn parse_addr_rejects_garbage_as_notfound() {
        assert!(matches!(
            RealTapeDataPlane::parse_addr("not a path"),
            Err(TapeError::NotFound(_))
        ));
        // A legal path parses.
        assert!(RealTapeDataPlane::parse_addr("corpus/chunk/7").is_ok());
    }

    #[test]
    fn kind_translation_is_total() {
        let mk = |k: TapePageKind| {
            Page::new(
                PageAddress::Chunk { chunk_id: 1 },
                "x".into(),
                context_tape::PageMeta {
                    kind: k,
                    est_tokens: 1,
                    importance: 0.5,
                    dirty: false,
                },
            )
        };
        assert_eq!(kind_of(&mk(TapePageKind::FileChunk)), PageKind::FileChunk);
        assert_eq!(
            kind_of(&mk(TapePageKind::MemoryObservation)),
            PageKind::MemoryObservation
        );
        assert_eq!(
            kind_of(&mk(TapePageKind::SummaryNode)),
            PageKind::SummaryNode
        );
        assert_eq!(kind_of(&mk(TapePageKind::Scratch)), PageKind::FileChunk);
    }
}
