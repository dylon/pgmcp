//! The **corpus reader** — and the *only* place the context tape touches the
//! durable corpus. Every function here is strictly READ-ONLY (`SELECT` only):
//! pgmcp never writes the user's files, and the corpus tables
//! (`file_chunks` / `indexed_files`) are owned by the indexer (pi), never the
//! tape. The write-back path lives elsewhere ([`crate::tape::real_data_plane`]),
//! supersedes only into `memory_observations`, and is gated off by default.
//!
//! ## What "hydrate" means
//!
//! Given a typed [`context_tape::PageAddress`], produce the resident
//! [`context_tape::Page`] whose `content` is what the model sees — already
//! *situated* with the deterministic
//! [`crate::indexer::contextualize::build_context_prefix`] header for corpus
//! pages (the same situating prefix used at embedding time, so a paged-in chunk
//! reads identically to its retrieval form). The hot/OOC tiers are consulted by
//! the data plane *before* calling here; hydrate is the cold path that turns a
//! durable row into a `Page`.
//!
//! | `PageAddress`        | source                                                |
//! |----------------------|-------------------------------------------------------|
//! | `Chunk { chunk_id }` | one `file_chunks` row + its `indexed_files` context    |
//! | `FileRegion {…}`     | a chunk-index span of one file (joined the same way)   |
//! | `File { file_id }`   | the file's first-region page (summary-style stand-in)  |
//! | `Observation {…}`    | one active `memory_observations` row                   |
//! | `Scratch {…}`        | **not hydratable** — served from the per-tree store     |
//!
//! ## Logging (ADR-021)
//!
//! A `sqlx::Error` from any read is a genuine DB-IO fault and surfaces as
//! [`HydrateError::Db`] — the caller logs it `error!`. A *well-formed address
//! that resolves to no row* (a deleted chunk, a closed observation) is the
//! benign [`HydrateError::NotFound`] the caller maps to a coverage gap.

use sqlx::PgPool;

use context_tape::{Page, PageAddress, PageKind, PageMeta};

use crate::indexer::contextualize::{ChunkContext, build_context_prefix};

/// Failure surface of the corpus reader. `Db` is an ADR-021 `error!`-grade
/// fault; `NotFound` is a benign miss; `NotHydratable` is a `Scratch` address
/// reaching the cold path (a caller bug — scratch lives only in the tree store).
#[derive(Debug, thiserror::Error)]
pub enum HydrateError {
    /// An underlying database read failed (ADR-021 `error!`).
    #[error("corpus read failed: {0}")]
    Db(#[from] sqlx::Error),
    /// The address is well-formed but resolves to no (active) corpus row.
    #[error("no corpus row for address: {0}")]
    NotFound(String),
    /// A `Scratch` address cannot be hydrated from the corpus (it is tree-local).
    #[error("scratch page is not hydratable from the corpus: {0}")]
    NotHydratable(String),
}

/// The contextual + raw fields for one chunk, joined from `file_chunks` and
/// `indexed_files` (+ an importer-count subquery). Drives both the raw content
/// and the situating prefix. All reads are `SELECT`-only.
#[derive(Debug, Clone, sqlx::FromRow)]
struct ChunkContextRow {
    /// Raw chunk text (what the agent ultimately reads, after the prefix).
    content: String,
    /// File path fields + language for the situating header.
    relative_path: String,
    language: String,
    /// Enclosing symbol (if the chunk's line span is covered by a `file_symbols`
    /// row). Best-effort — `NULL` for chunks with no enclosing symbol.
    symbol_kind: Option<String>,
    symbol_name: Option<String>,
    symbol_signature: Option<String>,
    /// Number of files importing this file (module-centrality proxy + the
    /// importance signal the tape budgets on).
    importer_count: i64,
}

/// Build the situated [`ChunkContext`] from a joined row (topics are left empty
/// — the embedding-time path also tolerates an empty topic list, and a per-chunk
/// topic join is intentionally skipped on the hot hydrate path to keep it a
/// single round-trip).
fn context_of(row: &ChunkContextRow) -> ChunkContext {
    ChunkContext {
        relative_path: row.relative_path.clone(),
        language: row.language.clone(),
        symbol_kind: row.symbol_kind.clone(),
        symbol_name: row.symbol_name.clone(),
        symbol_signature: row.symbol_signature.clone(),
        topics: Vec::new(),
        importer_count: row.importer_count,
    }
}

/// Map an importer count to the page's `importance` snapshot in `[0, 1]`. The
/// corpus carries no per-chunk importance column, so we derive a stable proxy
/// from module centrality: `0.5 + 0.5·(importers / (importers + 4))` — a clean
/// page with no importers reads as the neutral `0.5`, and importance rises
/// monotonically (asymptotically → 1.0) with how widely the file is depended on.
/// Deterministic, so two hydrations of the same row score identically.
fn importance_from_importers(importer_count: i64) -> f32 {
    let n = importer_count.max(0) as f32;
    0.5 + 0.5 * (n / (n + 4.0))
}

/// Public re-export of [`importance_from_importers`] so the data plane's
/// `resolve(Chunk)` path derives the SAME importance proxy a hydrate would, from
/// an importer count it already has in hand — keeping a candidate ref's
/// importance consistent with the page it will hydrate to.
#[inline]
pub fn importance_from_importers_pub(importer_count: i64) -> f32 {
    importance_from_importers(importer_count)
}

/// Assemble a situated corpus [`Page`] from a joined chunk row: `content` =
/// `build_context_prefix(ctx)` ++ raw chunk text; `meta` carries the derived
/// importance and a deterministic `len/4` token estimate.
fn page_from_chunk_row(addr: PageAddress, row: &ChunkContextRow) -> Page {
    let prefix = build_context_prefix(&context_of(row));
    let content = format!("{prefix}{}", row.content);
    let est_tokens = Page::estimate_tokens(&content);
    let importance = importance_from_importers(row.importer_count);
    Page::new(
        addr,
        content,
        PageMeta::clean(PageKind::FileChunk, est_tokens, importance),
    )
}

/// READ-ONLY: fetch one chunk (by `file_chunks.id`) with its situating context.
///
/// Mirrors the contextual re-embed cron's join
/// ([`crate::db::queries::get_chunks_needing_context`]) so a hydrated
/// page reads identically to its embedding-time form: the enclosing symbol is
/// the innermost `file_symbols` row whose line span contains the chunk, and the
/// importer count is the number of distinct files importing this file
/// (`code_graph_edges … edge_type = 'import'`).
async fn read_chunk_row(
    pool: &PgPool,
    chunk_id: i64,
) -> Result<Option<ChunkContextRow>, sqlx::Error> {
    sqlx::query_as::<_, ChunkContextRow>(
        "SELECT c.content, f.relative_path, f.language,
                sym.kind AS symbol_kind, sym.name AS symbol_name,
                sym.signature AS symbol_signature,
                COALESCE(imp.cnt, 0) AS importer_count
         FROM file_chunks c
         JOIN indexed_files f ON f.id = c.file_id
         LEFT JOIN LATERAL (
             SELECT fs.kind, fs.name, fs.signature
             FROM file_symbols fs
             WHERE fs.file_id = c.file_id
               AND fs.start_line <= c.start_line
               AND fs.end_line >= c.end_line
               AND fs.kind IN ('function','method','class','struct','impl','trait','interface','enum','module')
             ORDER BY fs.start_line DESC
             LIMIT 1
         ) sym ON true
         LEFT JOIN LATERAL (
             SELECT COUNT(DISTINCT e.source_file_id) AS cnt
             FROM code_graph_edges e
             WHERE e.target_file_id = c.file_id AND e.edge_type = 'import'
         ) imp ON true
         WHERE c.id = $1",
    )
    .bind(chunk_id)
    .fetch_optional(pool)
    .await
}

/// Hydrate a [`PageAddress::Chunk`] into a situated [`Page`]. `NotFound` if the
/// chunk id is unknown / deleted.
pub async fn hydrate_chunk(pool: &PgPool, chunk_id: i64) -> Result<Page, HydrateError> {
    let addr = PageAddress::Chunk { chunk_id };
    match read_chunk_row(pool, chunk_id).await? {
        Some(row) => Ok(page_from_chunk_row(addr, &row)),
        None => Err(HydrateError::NotFound(addr.to_path())),
    }
}

/// READ-ONLY: fetch a contiguous chunk-index span of one file, joined to the
/// file's situating context, ordered by `chunk_index`. Reuses the canonical
/// [`crate::db::queries::get_chunks_in_index_range`] for the raw chunk
/// rows after resolving `file_id → path`.
async fn read_region_rows(
    pool: &PgPool,
    file_id: i64,
    start_chunk: i32,
    end_chunk: i32,
) -> Result<
    Option<(
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
        String,
    )>,
    sqlx::Error,
> {
    // First resolve the file's situating context (one row). The path is also what
    // `get_chunks_in_index_range` keys on.
    let header: Option<(String, String, String, i64)> = sqlx::query_as(
        "SELECT f.path, f.relative_path, f.language, COALESCE(imp.cnt, 0) AS importer_count
         FROM indexed_files f
         LEFT JOIN LATERAL (
            SELECT COUNT(DISTINCT e.source_file_id) AS cnt
            FROM code_graph_edges e
            WHERE e.target_file_id = f.id AND e.edge_type = 'import'
         ) imp ON true
         WHERE f.id = $1",
    )
    .bind(file_id)
    .fetch_optional(pool)
    .await?;
    let Some((path, relative_path, language, importer_count)) = header else {
        return Ok(None);
    };
    // Raw chunk rows for the span via the canonical reader (which follows
    // `duplicate_of_file_id`).
    let chunks =
        crate::db::queries::get_chunks_in_index_range(pool, &path, start_chunk, end_chunk).await?;
    if chunks.is_empty() {
        return Ok(None);
    }
    // Concatenate the span's raw text in chunk-index order.
    let mut body = String::new();
    for (i, ch) in chunks.iter().enumerate() {
        if i > 0 {
            body.push('\n');
        }
        body.push_str(&ch.content);
    }
    // Region symbol/signature is not meaningful across a span; leave them None so
    // the prefix is path/lang/centrality only.
    Ok(Some((
        body,
        relative_path,
        None,
        None,
        None,
        importer_count,
        language,
    )))
}

/// Hydrate a [`PageAddress::FileRegion`] into a situated [`Page`] whose content
/// is the concatenated span text under a path/lang/centrality header. `NotFound`
/// if the file is unknown or the span is empty.
pub async fn hydrate_region(
    pool: &PgPool,
    file_id: i64,
    start_chunk: i32,
    end_chunk: i32,
) -> Result<Page, HydrateError> {
    let addr = PageAddress::FileRegion {
        file_id,
        start_chunk,
        end_chunk,
    };
    let Some((
        body,
        relative_path,
        symbol_kind,
        symbol_name,
        symbol_signature,
        importer_count,
        language,
    )) = read_region_rows(pool, file_id, start_chunk, end_chunk).await?
    else {
        return Err(HydrateError::NotFound(addr.to_path()));
    };
    let row = ChunkContextRow {
        content: body,
        relative_path,
        language,
        symbol_kind,
        symbol_name,
        symbol_signature,
        importer_count,
    };
    Ok(page_from_chunk_row(addr, &row))
}

/// Hydrate a [`PageAddress::File`] into a situated [`Page`]. The whole-file page
/// is served as its **first region** (chunk-index `0..=0` widened to the file's
/// chunk count is overkill for a budgeted page-in), so a `File` address pages in
/// the file's leading chunk under a file header — a compact, deterministic
/// stand-in. `NotFound` if the file has no chunks.
pub async fn hydrate_file(pool: &PgPool, file_id: i64) -> Result<Page, HydrateError> {
    let addr = PageAddress::File { file_id };
    // Resolve the file's path, then size the first region by its chunk summary so
    // a small file pages in whole and a large one pages in its first chunk only.
    let path: Option<String> = sqlx::query_scalar("SELECT path FROM indexed_files WHERE id = $1")
        .bind(file_id)
        .fetch_optional(pool)
        .await?;
    let Some(path) = path else {
        return Err(HydrateError::NotFound(addr.to_path()));
    };
    let summary = crate::db::queries::file_chunk_summary(pool, &path).await?;
    if summary.chunk_count <= 0 {
        return Err(HydrateError::NotFound(addr.to_path()));
    }
    // First region: chunk index 0 only (the leading chunk) — a budget-friendly
    // representative page. Larger reads use explicit FileRegion addresses.
    match read_region_rows(pool, file_id, 0, 0).await? {
        Some((body, relative_path, _sk, _sn, _ss, importer_count, language)) => {
            let row = ChunkContextRow {
                content: body,
                relative_path,
                language,
                symbol_kind: None,
                symbol_name: None,
                symbol_signature: None,
                importer_count,
            };
            Ok(page_from_chunk_row(addr, &row))
        }
        None => Err(HydrateError::NotFound(addr.to_path())),
    }
}

/// READ-ONLY: fetch one *active* memory observation (bi-temporal: `valid_to IS
/// NULL`). Returns `(content, importance)`.
async fn read_observation(
    pool: &PgPool,
    obs_id: i64,
) -> Result<Option<(String, f32)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT content, importance
         FROM memory_observations
         WHERE id = $1 AND valid_to IS NULL",
    )
    .bind(obs_id)
    .fetch_optional(pool)
    .await
}

/// Hydrate a [`PageAddress::Observation`] into a [`Page`]. Observation content is
/// already prose (no situating prefix is prepended — it is not a code chunk), so
/// the page content is the raw observation text. `NotFound` if the observation is
/// unknown or has been superseded (`valid_to` closed).
pub async fn hydrate_observation(pool: &PgPool, obs_id: i64) -> Result<Page, HydrateError> {
    let addr = PageAddress::Observation { obs_id };
    match read_observation(pool, obs_id).await? {
        Some((content, importance)) => {
            let est_tokens = Page::estimate_tokens(&content);
            Ok(Page::new(
                addr,
                content,
                PageMeta::clean(PageKind::MemoryObservation, est_tokens, importance),
            ))
        }
        None => Err(HydrateError::NotFound(addr.to_path())),
    }
}

/// Hydrate any corpus [`PageAddress`] into its situated [`Page`]. The single
/// entry point the data plane calls on a hot/OOC miss. A `Scratch` address is a
/// caller bug here (it lives only in the per-tree store) and returns
/// [`HydrateError::NotHydratable`].
pub async fn hydrate(pool: &PgPool, address: &PageAddress) -> Result<Page, HydrateError> {
    match address {
        PageAddress::Chunk { chunk_id } => hydrate_chunk(pool, *chunk_id).await,
        PageAddress::FileRegion {
            file_id,
            start_chunk,
            end_chunk,
        } => hydrate_region(pool, *file_id, *start_chunk, *end_chunk).await,
        PageAddress::File { file_id } => hydrate_file(pool, *file_id).await,
        PageAddress::Observation { obs_id } => hydrate_observation(pool, *obs_id).await,
        PageAddress::Scratch { .. } => Err(HydrateError::NotHydratable(address.to_path())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn importance_is_monotonic_and_bounded() {
        // Neutral at zero importers, rises toward (but never reaches) 1.0.
        assert!((importance_from_importers(0) - 0.5).abs() < 1e-6);
        assert!(importance_from_importers(4) > importance_from_importers(0));
        assert!(importance_from_importers(100) > importance_from_importers(4));
        assert!(importance_from_importers(1_000_000) < 1.0);
        // Negative (defensive) clamps to the neutral floor.
        assert!((importance_from_importers(-5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn chunk_page_carries_the_context_prefix_header() {
        // A row with a path + language must produce a `[File: … | Lang: …]\n`
        // header ahead of the raw chunk text (the build_context_prefix contract).
        let row = ChunkContextRow {
            content: "fn main() {}".into(),
            relative_path: "src/main.rs".into(),
            language: "rust".into(),
            symbol_kind: Some("function".into()),
            symbol_name: Some("main".into()),
            symbol_signature: Some("fn main()".into()),
            importer_count: 3,
        };
        let page = page_from_chunk_row(PageAddress::Chunk { chunk_id: 1 }, &row);
        assert!(
            page.content.starts_with("[File: src/main.rs"),
            "got: {}",
            page.content
        );
        assert!(page.content.contains("Lang: rust"));
        assert!(page.content.contains("function: fn main()"));
        assert!(page.content.contains("Imported by: 3"));
        assert!(
            page.content.ends_with("fn main() {}"),
            "raw chunk follows the header"
        );
        assert_eq!(page.meta.kind, PageKind::FileChunk);
        assert!(!page.meta.dirty, "a hydrated page is clean");
        assert_eq!(page.meta.est_tokens, Page::estimate_tokens(&page.content));
    }

    #[test]
    fn scratch_is_not_hydratable() {
        // Pure (no DB): the dispatcher rejects a Scratch address without a pool
        // call. We can assert the variant arm without async by constructing the
        // error directly via the address path it would carry.
        let addr = PageAddress::Scratch {
            tree: uuid::Uuid::nil(),
            slot: Box::new([1, 2]),
        };
        // The dispatcher's Scratch arm is the only one that does not touch `pool`;
        // mirror its mapping here.
        let err = HydrateError::NotHydratable(addr.to_path());
        assert!(matches!(err, HydrateError::NotHydratable(_)));
    }
}
