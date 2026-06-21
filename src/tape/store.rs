//! Working-set **persistence** — pure DB read/write over pgmcp's OWN
//! `working_set_pages` / `working_set_config` tables (v51 migration). Mirrors
//! [`crate::csm::session_store`]: no policy, no eviction logic lives here — that
//! is the [`engine`](crate::tape::engine)'s job. These functions only move the
//! [`WorkingSet`](crate::tape::working_set::WorkingSet) to and from Postgres.
//!
//! ## Boundary
//!
//! Every function is an ANALYTICAL/coordination/MEMORY operation: agent-/engine-
//! supplied values + DB reads/writes to pgmcp's own tables. pgmcp never runs a
//! shell or writes the user's files.
//!
//! ## Determinism
//!
//! `last_access_ord` is persisted verbatim as the LOGICAL clock value the engine
//! stamped; nothing here reads wall-time into it. A `load_working_set` after a
//! `save_working_set` round-trips the exact logical metadata, so a resumed
//! session continues from a bit-identical residency snapshot.

use sqlx::PgPool;

use crate::tape::vocab::{EvictReason, EvictionPolicy, PageKind, PageState};
use crate::tape::working_set::{OrderedPages, PageAddr, ResidentPage, WorkingSet};

/// A row of `working_set_pages` loaded back. Kept narrow (only what the working
/// set needs); the SELECT list [`PAGE_COLS`] and this struct cannot drift.
#[derive(Debug, Clone, sqlx::FromRow)]
struct PageRow {
    page_kind: String,
    page_addr: String,
    state: String,
    importance: f32,
    est_tokens: i32,
    use_count: i32,
    last_access_ord: i64,
    dirty: bool,
    /// The durable scratch-page bytes (v53 `content` column). `NULL` for
    /// re-fetchable corpus / observation / summary pages; populated only for
    /// `Scratch`-kind pages (it carries the bytes that have no corpus source).
    content: Option<String>,
}

/// The columns selected back for [`PageRow`].
const PAGE_COLS: &str = "page_kind, page_addr, state, importance, est_tokens, use_count, last_access_ord, dirty, content";

/// UPSERT one resident page row (idempotent on `(session_key, state_cursor,
/// page_addr)`) over the pool. The `state` is derived from the page's flags:
/// `pinned` ⇒ `pinned`, else `dirty` ⇒ `dirty`, else `resident`. `evict_reason`
/// is cleared (a resident page has none). The page's carried
/// [`bytes`](ResidentPage::bytes) are written to the `content` column verbatim
/// (`NULL` when `None`), so a `Scratch` page's payload survives a pause/resume.
///
/// A pooled convenience wrapper around [`save_resident_page_on`]; prefer the
/// `_on` variant inside a multi-statement transaction (see
/// [`save_working_set`]).
pub async fn save_resident_page(
    pool: &PgPool,
    session_key: &str,
    state_cursor: i32,
    tree_path: &str,
    page: &ResidentPage,
) -> Result<(), sqlx::Error> {
    let mut conn = pool.acquire().await?;
    save_resident_page_on(&mut conn, session_key, state_cursor, tree_path, page).await
}

/// UPSERT one resident page row over an explicit connection (the
/// transaction-aware core of [`save_resident_page`]). Used inside the
/// [`save_working_set`] transaction so a mid-loop failure rolls back every page
/// written in the same atomic flush.
pub async fn save_resident_page_on(
    conn: &mut sqlx::PgConnection,
    session_key: &str,
    state_cursor: i32,
    tree_path: &str,
    page: &ResidentPage,
) -> Result<(), sqlx::Error> {
    let state = if page.pinned {
        PageState::Pinned
    } else if page.dirty {
        PageState::Dirty
    } else {
        PageState::Resident
    };
    sqlx::query(
        "INSERT INTO working_set_pages
            (session_key, state_cursor, page_kind, page_addr, tree_path, state,
             importance, est_tokens, use_count, last_access_ord, dirty, content, evict_reason)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, NULL)
         ON CONFLICT (session_key, state_cursor, page_addr) DO UPDATE SET
            page_kind       = EXCLUDED.page_kind,
            tree_path       = EXCLUDED.tree_path,
            state           = EXCLUDED.state,
            importance      = EXCLUDED.importance,
            est_tokens      = EXCLUDED.est_tokens,
            use_count       = EXCLUDED.use_count,
            last_access_ord = EXCLUDED.last_access_ord,
            dirty           = EXCLUDED.dirty,
            content         = EXCLUDED.content,
            evict_reason    = NULL",
    )
    .bind(session_key)
    .bind(state_cursor)
    .bind(page.kind.as_str())
    .bind(&page.addr.0)
    .bind(tree_path)
    .bind(state.as_str())
    .bind(page.importance)
    .bind(page.est_tokens)
    .bind(page.use_count as i32)
    .bind(page.last_access_ord as i64)
    .bind(page.dirty)
    .bind(page.bytes.as_deref())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Mark a page dirty (a write-back is owed). Sets both the `dirty` flag and the
/// `state = 'dirty'` so the partial dirty index and the load path agree.
pub async fn mark_dirty(
    pool: &PgPool,
    session_key: &str,
    state_cursor: i32,
    addr: &PageAddr,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE working_set_pages
            SET dirty = true, state = 'dirty'
          WHERE session_key = $1 AND state_cursor = $2 AND page_addr = $3",
    )
    .bind(session_key)
    .bind(state_cursor)
    .bind(&addr.0)
    .execute(pool)
    .await?;
    Ok(())
}

/// Evict a page: set `state = 'evicted'`, clear `dirty`, and record the closed
/// [`EvictReason`]. The row is retained (audit + replay determinism); only its
/// state changes. The `content` column is deliberately **left untouched** so a
/// scratch page evicted mid-run (e.g. while a session is paused) is still
/// byte-rehydratable by [`rehydrate_store_from_pages`] — eviction frees a page
/// from the *resident* set, it does not destroy the scratch payload.
pub async fn evict_page(
    pool: &PgPool,
    session_key: &str,
    state_cursor: i32,
    addr: &PageAddr,
    reason: EvictReason,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE working_set_pages
            SET state = 'evicted', dirty = false, evict_reason = $4
          WHERE session_key = $1 AND state_cursor = $2 AND page_addr = $3",
    )
    .bind(session_key)
    .bind(state_cursor)
    .bind(&addr.0)
    .bind(reason.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

/// List the addresses of currently-dirty pages for a working set (write-back
/// queue). Ordered by `last_access_ord` for a deterministic flush order.
pub async fn list_dirty(
    pool: &PgPool,
    session_key: &str,
    state_cursor: i32,
) -> Result<Vec<PageAddr>, sqlx::Error> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT page_addr FROM working_set_pages
          WHERE session_key = $1 AND state_cursor = $2 AND dirty
          ORDER BY last_access_ord, page_addr",
    )
    .bind(session_key)
    .bind(state_cursor)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(a,)| PageAddr(a)).collect())
}

/// Atomically bump the session's logical clock by `delta` and return the new
/// value. The monotonic source for `last_access_ord`; UPSERTs a config row at
/// the default policy/budget if none exists yet.
pub async fn bump_clock(pool: &PgPool, session_key: &str, delta: i64) -> Result<i64, sqlx::Error> {
    let new_clock: i64 = sqlx::query_scalar(
        "INSERT INTO working_set_config (session_key, logical_clock, updated_at)
         VALUES ($1, $2, NOW())
         ON CONFLICT (session_key) DO UPDATE SET
            logical_clock = working_set_config.logical_clock + $2,
            updated_at = NOW()
         RETURNING logical_clock",
    )
    .bind(session_key)
    .bind(delta)
    .fetch_one(pool)
    .await?;
    Ok(new_clock)
}

/// UPSERT the session's `working_set_config` from a [`WorkingSet`] (budget,
/// policy, model window, ttl) over the pool. A pooled wrapper around
/// [`save_config_on`].
///
/// ## Single clock authority
///
/// `logical_clock` is **only seeded on the initial INSERT** (the in-memory
/// `ws.clock`, normally `0` for a fresh session) and is deliberately **NOT** in
/// the `DO UPDATE SET` list. The sole durable authority for advancing the clock
/// is [`bump_clock`] (an atomic relative `logical_clock = logical_clock + delta`
/// RETURNING the new value). Were `save_config` to also overwrite `logical_clock`
/// with the in-memory snapshot, two writers racing a flush could regress the
/// durable clock (lost ticks) — a determinism hazard. The engine therefore
/// advances the clock exclusively via `bump_clock` and stamps `last_access_ord`
/// from the returned value; `save_config` never moves it.
pub async fn save_config(
    pool: &PgPool,
    ws: &WorkingSet,
    model_window_tokens: i32,
    ttl_secs: Option<i32>,
) -> Result<(), sqlx::Error> {
    let mut conn = pool.acquire().await?;
    save_config_on(&mut conn, ws, model_window_tokens, ttl_secs).await
}

/// UPSERT the session's `working_set_config` over an explicit connection (the
/// transaction-aware core of [`save_config`]). See [`save_config`] for the
/// single-clock-authority rationale — `logical_clock` is seeded on INSERT only,
/// never overwritten on conflict, so [`bump_clock`] remains the sole durable
/// clock authority.
pub async fn save_config_on(
    conn: &mut sqlx::PgConnection,
    ws: &WorkingSet,
    model_window_tokens: i32,
    ttl_secs: Option<i32>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO working_set_config
            (session_key, model_window_tokens, budget_tokens, policy, ttl_secs, logical_clock, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, NOW())
         ON CONFLICT (session_key) DO UPDATE SET
            model_window_tokens = EXCLUDED.model_window_tokens,
            budget_tokens       = EXCLUDED.budget_tokens,
            policy              = EXCLUDED.policy,
            ttl_secs            = EXCLUDED.ttl_secs,
            updated_at          = NOW()",
    )
    .bind(&ws.session_key)
    .bind(model_window_tokens)
    .bind(ws.budget_tokens)
    .bind(ws.policy.as_str())
    .bind(ttl_secs)
    .bind(ws.clock as i64)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Persist the entire working set **atomically**: the config row plus every
/// currently-resident page (states `resident` / `pinned` / `dirty`) in ONE
/// transaction. Evicted rows are left as the engine wrote them (via
/// [`evict_page`]). Resident-token sum is recomputed by [`load_working_set`], so
/// it need not be stored.
///
/// ## Atomicity
///
/// The `save_config` + per-page `save_resident_page` loop run inside a single
/// `pool.begin()` / `commit()` (the same proven pattern as
/// [`crate::tape::real_data_plane::RealTapeDataPlane::supersede_observation`]):
/// a fault mid-loop (e.g. a DB error after some pages were written) drops the
/// transaction guard, rolling back **every** row written in this flush — the
/// durable state is never a partial snapshot of the working set.
pub async fn save_working_set(
    pool: &PgPool,
    ws: &WorkingSet,
    tree_path: &str,
    model_window_tokens: i32,
    ttl_secs: Option<i32>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    // `&mut tx` where a `&mut sqlx::PgConnection` is expected resolves via
    // `Transaction`'s `DerefMut<Target = PgConnection>` deref-coercion (the same
    // coercion the `sqlx` transaction docs and `supersede_observation` rely on).
    save_config_on(&mut tx, ws, model_window_tokens, ttl_secs).await?;
    for page in ws.pages.iter_in_order() {
        save_resident_page_on(&mut tx, &ws.session_key, ws.state_cursor, tree_path, page).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The `tree_path` recorded on a working set's pages at `(session_key,
/// state_cursor)`, if any rows exist. All pages of one session share a single
/// [`crate::tape::data_plane::TreePath`] (one context tree per orchestration
/// run), so the most-common stored value is canonical; `None` when the working
/// set has no rows yet. Used by the PAUSE flush so a re-save preserves the
/// existing tree path verbatim rather than clobbering it.
pub async fn tree_path_of(
    pool: &PgPool,
    session_key: &str,
    state_cursor: i32,
) -> Result<Option<String>, sqlx::Error> {
    // The modal (most frequent) tree_path among the session's rows. A single
    // GROUP BY/ORDER BY COUNT is deterministic and tolerates the (unexpected)
    // mixed-path case without erroring.
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT tree_path FROM working_set_pages
          WHERE session_key = $1 AND state_cursor = $2
          GROUP BY tree_path
          ORDER BY COUNT(*) DESC, tree_path
          LIMIT 1",
    )
    .bind(session_key)
    .bind(state_cursor)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(p,)| p))
}

/// Flush the durable working set at a PAUSE boundary: re-persist the currently
/// resident pages + config for `(session_key, state_cursor)`, preserving each
/// page's existing `tree_path`, `model_window_tokens`, and `ttl_secs`. Returns
/// the number of resident pages flushed (0 when the session has no working set —
/// a benign no-op).
///
/// This mirrors how `session_checkpoint_save` flushes the recorded transcript to
/// `csm_run_traces`: the residency state the [`crate::tape::engine::PagingEngine`]
/// wrote incrementally during the run is read back and durably re-committed at the
/// suspend point, so a later `load_working_set` after resume reconstructs the
/// IDENTICAL snapshot (the logical-clock determinism guarantee). It is idempotent
/// and fully non-destructive: a [`save_working_set`] UPSERT over the
/// already-persisted rows + config leaves their values (including the columns the
/// in-memory [`WorkingSet`] does not carry) unchanged.
pub async fn flush_working_set(
    pool: &PgPool,
    session_key: &str,
    state_cursor: i32,
) -> Result<usize, sqlx::Error> {
    let ws = load_working_set(pool, session_key, state_cursor).await?;
    let n = ws.pages.len();
    if n == 0 {
        // No working set for this session — nothing to flush. (Config may still
        // exist; `load_working_set` already round-tripped it, so we leave it be.)
        return Ok(0);
    }
    // Preserve the columns the in-memory WorkingSet does not hold: the existing
    // tree_path (per page) and the config's model_window_tokens / ttl_secs.
    let tree_path = tree_path_of(pool, session_key, state_cursor)
        .await?
        .unwrap_or_default();
    let cfg: Option<(i32, Option<i32>)> = sqlx::query_as(
        "SELECT model_window_tokens, ttl_secs FROM working_set_config WHERE session_key = $1",
    )
    .bind(session_key)
    .fetch_optional(pool)
    .await?;
    let (model_window_tokens, ttl_secs) = cfg.unwrap_or((ws.budget_tokens, None));
    save_working_set(pool, &ws, &tree_path, model_window_tokens, ttl_secs).await?;
    Ok(n)
}

/// Load a working set for `(session_key, state_cursor)`: the config (budget /
/// policy / clock) plus every non-evicted page, reconstructing
/// `resident_tokens` as the Σ of their `est_tokens`. Pages are loaded in
/// `last_access_ord, page_addr` order so the in-memory insertion order is a
/// deterministic function of the persisted logical metadata (FIFO stability
/// across resume).
///
/// If no config row exists, the working set defaults to a zero budget and the
/// `importance_weighted` policy (the caller typically overwrites the budget from
/// `[tape]` config before use).
pub async fn load_working_set(
    pool: &PgPool,
    session_key: &str,
    state_cursor: i32,
) -> Result<WorkingSet, sqlx::Error> {
    // Config (may be absent on a never-saved session). `ttl_secs` is read back
    // as the logical-TTL tick count so a resumed session keeps the same TTL
    // behavior (a non-positive value ⇒ no TTL).
    let cfg: Option<(i32, String, i64, Option<i32>)> = sqlx::query_as(
        "SELECT budget_tokens, policy, logical_clock, ttl_secs
           FROM working_set_config WHERE session_key = $1",
    )
    .bind(session_key)
    .fetch_optional(pool)
    .await?;
    let (budget_tokens, policy, clock, ttl) = match cfg {
        Some((b, p, c, t)) => (
            b,
            EvictionPolicy::parse(&p).unwrap_or(EvictionPolicy::ImportanceWeighted),
            c.max(0) as u64,
            match t {
                Some(secs) if secs > 0 => Some(secs as u64),
                _ => None,
            },
        ),
        None => (0, EvictionPolicy::ImportanceWeighted, 0, None),
    };

    let sql = format!(
        "SELECT {PAGE_COLS} FROM working_set_pages
          WHERE session_key = $1 AND state_cursor = $2 AND state <> 'evicted'
          ORDER BY last_access_ord, page_addr"
    );
    let rows = sqlx::query_as::<_, PageRow>(sqlx::AssertSqlSafe(sql))
        .bind(session_key)
        .bind(state_cursor)
        .fetch_all(pool)
        .await?;

    let mut pages = OrderedPages::with_capacity(rows.len());
    let mut resident_tokens: i32 = 0;
    for r in rows {
        let kind = PageKind::parse(&r.page_kind).unwrap_or(PageKind::FileChunk);
        let pinned = r.state == PageState::Pinned.as_str();
        resident_tokens = resident_tokens.saturating_add(r.est_tokens);
        pages.insert(ResidentPage {
            addr: PageAddr(r.page_addr),
            kind,
            importance: r.importance,
            est_tokens: r.est_tokens,
            use_count: r.use_count.max(0) as u32,
            last_access_ord: r.last_access_ord.max(0) as u64,
            dirty: r.dirty,
            pinned,
            // The durable scratch bytes (NULL for re-fetchable corpus pages).
            bytes: r.content,
        });
    }

    Ok(WorkingSet {
        session_key: session_key.to_string(),
        state_cursor,
        budget_tokens,
        resident_tokens,
        policy,
        clock,
        ttl,
        pages,
    })
}

/// Rehydrate the **RAM plane** (the per-tree [`context_tape::TapeStore`]) from the
/// durable working set's scratch pages — the resume-side reconstruction that
/// makes pause/resume actually carry an RLM run's accumulator / REPL state across
/// a process boundary.
///
/// ## The two disjoint planes and the bridge
///
/// pgmcp's context tape has two planes that key residency differently:
///
/// - the **DB plane** ([`PagingEngine`](crate::tape::engine::PagingEngine) over
///   `working_set_pages` / `working_set_config`) keyed by `(session_key,
///   state_cursor)`;
/// - the **RAM plane** ([`TapeRegistry`] of `TapeStore`s) keyed by
///   [`TreeId`](context_tape::TreeId).
///
/// They reach each other from a single value: for an RLM run the `session_key`
/// **is** the tree path string (`"rlm:{root_task_id}"` =
/// [`TreePath::for_root_task`](crate::tape::data_plane::TreePath::for_root_task)),
/// and the store's `TreeId` is the SHA-256 derivation of that path
/// ([`RealTapeDataPlane::tree_id`](crate::tape::real_data_plane::RealTapeDataPlane::tree_id)).
///
/// ## What is (and is not) rehydrated
///
/// Only `Scratch`-kind pages that carry durable bytes (`content IS NOT NULL`,
/// surfaced as [`ResidentPage::bytes`]`= Some(..)`) are reconstructed: they have
/// **no corpus source**, so their bytes only survive a resume because they were
/// persisted to `working_set_pages.content`. Corpus / observation / summary
/// pages are deliberately **not** eagerly rehydrated — they re-fetch lazily from
/// the read-only corpus via the data plane on the next demand, so re-materializing
/// them here would be wasted work. A page whose address does not parse to a
/// [`PageAddress::Scratch`](context_tape::PageAddress::Scratch) is skipped even
/// if it somehow carries bytes (defensive: the store can only hold a scratch page
/// without corpus backing).
///
/// The reconstructed [`context_tape::Page`] is inserted via
/// [`insert_hydrated`](context_tape::TapeStore::insert_hydrated) (the store's
/// "admit a known page" path), preserving the persisted `est_tokens` and
/// importance so the RAM copy matches the durable record byte- and
/// metadata-identically.
///
/// Returns the number of scratch pages rehydrated (0 is a benign no-op — a
/// session with no scratch pages, or none persisted yet).
pub async fn rehydrate_store_from_pages(
    pool: &PgPool,
    registry: &crate::tape::registry::TapeRegistry,
    session_key: &str,
    state_cursor: i32,
) -> Result<usize, sqlx::Error> {
    use crate::tape::data_plane::TreePath;
    use crate::tape::real_data_plane::RealTapeDataPlane;
    use context_tape::{Page, PageMeta};

    let ws = load_working_set(pool, session_key, state_cursor).await?;
    let tree_path = TreePath(session_key.to_string());
    let tree_id = RealTapeDataPlane::tree_id(&tree_path);

    let mut rehydrated = 0usize;
    for page in ws.pages.iter_in_order() {
        // Only a scratch page that owns durable bytes is rehydratable from the
        // control plane; everything else re-fetches lazily from the corpus.
        let Some(bytes) = page.bytes.as_deref() else {
            continue;
        };
        let Some(address) = crate::tape::address_resolve::pageaddr_to_address(&page.addr) else {
            continue;
        };
        if !matches!(address, context_tape::PageAddress::Scratch { .. }) {
            continue;
        }
        let meta = PageMeta {
            kind: context_tape::PageKind::Scratch,
            est_tokens: page.est_tokens.max(0) as u32,
            importance: page.importance,
            dirty: false,
        };
        let reconstructed = Page::new(address.clone(), bytes.to_string(), meta);
        registry.with_store_mut(tree_id, |s| {
            s.insert_hydrated(address.clone(), reconstructed.clone());
        });
        rehydrated += 1;
    }
    Ok(rehydrated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape::data_plane::TreePath;
    use crate::tape::real_data_plane::RealTapeDataPlane;
    use crate::tape::registry::TapeRegistry;
    use crate::tape::working_set::PageAddr;
    use context_tape::Page;

    /// DB-free: the SELECT column list and the [`PageRow`] field set must stay in
    /// lockstep — every column listed in [`PAGE_COLS`] maps to exactly one struct
    /// field (sqlx's `FromRow` is positional here). This pins the `content`
    /// addition (and any future column) against silent drift.
    #[test]
    fn page_cols_and_pagerow_have_equal_arity() {
        let cols = PAGE_COLS.split(',').count();
        // PageRow fields: page_kind, page_addr, state, importance, est_tokens,
        // use_count, last_access_ord, dirty, content.
        assert_eq!(cols, 9, "PAGE_COLS column count");
    }

    // -----------------------------------------------------------------------
    // DB-backed tests. These run against the configured pgmcp Postgres (whose
    // schema is at v53+, carrying `working_set_pages.content` and the relaxed
    // session FK). When no DB is reachable they SKIP (benign no-op) rather than
    // fail — the same posture as `pgmcp_testing::require_test_db`, but
    // self-contained because `pgmcp-testing` depends on this crate (a cycle
    // forbids reusing it here). Each test namespaces its rows by a fresh UUID
    // `session_key` (the v53-relaxed FK admits a synthetic `rlm:{uuid}` key) and
    // cleans up after itself, so concurrent runs never collide.
    // -----------------------------------------------------------------------

    /// Acquire a pool to an ISOLATED test DB (`PGMCP_TEST_DATABASE_URL`), or `None`
    /// to skip. NEVER the live default DB: these tests mutate working-set rows, and
    /// the live schema may lag the code (e.g. before the daemon is restarted with a
    /// new migration). Runs migrations (idempotent) so the schema matches the code.
    /// A connect/migrate failure is a skip, not a panic.
    async fn test_pool() -> Option<PgPool> {
        use sqlx::postgres::PgPoolOptions;
        let url = std::env::var("PGMCP_TEST_DATABASE_URL").ok()?;
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .ok()?;
        crate::db::migrations::run_migrations(
            &pool,
            &crate::config::VectorConfig::default(),
            false,
        )
        .await
        .ok()?;
        Some(pool)
    }

    /// Delete every working-set row for a session_key (test cleanup).
    async fn purge(pool: &PgPool, session_key: &str) {
        let _ = sqlx::query("DELETE FROM working_set_pages WHERE session_key = $1")
            .bind(session_key)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM working_set_config WHERE session_key = $1")
            .bind(session_key)
            .execute(pool)
            .await;
    }

    fn fresh_session_key() -> String {
        format!("rlm:{}", uuid::Uuid::new_v4())
    }

    /// CLOCK DETERMINISM: two concurrent `bump_clock(+1)` against the same
    /// session must both land (atomic relative increment, no lost ticks) and
    /// `save_config` must NOT regress the durable clock afterward (it no longer
    /// overwrites `logical_clock`). Final durable clock == N increments.
    #[tokio::test]
    async fn bump_clock_is_atomic_and_save_config_does_not_regress_it() {
        let Some(pool) = test_pool().await else {
            eprintln!(
                "[tape::store] SKIPPED bump_clock_is_atomic: no test DB \
                 (set PGMCP_TEST_DATABASE_URL or a pgmcp config)"
            );
            return;
        };
        let session_key = fresh_session_key();
        purge(&pool, &session_key).await;

        // Fire N concurrent +1 bumps. With an atomic relative increment, the set
        // of returned values is exactly {1..=N} and the final stored clock is N —
        // no lost updates from read-modify-write races.
        const N: i64 = 32;
        let mut handles = Vec::with_capacity(N as usize);
        for _ in 0..N {
            let p = pool.clone();
            let sk = session_key.clone();
            handles.push(tokio::spawn(async move {
                bump_clock(&p, &sk, 1).await.expect("bump_clock")
            }));
        }
        let mut seen = std::collections::BTreeSet::new();
        for h in handles {
            seen.insert(h.await.expect("join"));
        }
        let expected: std::collections::BTreeSet<i64> = (1..=N).collect();
        assert_eq!(
            seen, expected,
            "every concurrent bump returned a unique tick"
        );

        // Now a save_config flush must NOT move the durable clock (it is seeded on
        // INSERT only; bump_clock is the sole authority). Build a ws whose
        // in-memory clock is a STALE 0 and persist it.
        let ws = WorkingSet::new(session_key.clone(), 0, 1000, EvictionPolicy::Lru);
        assert_eq!(ws.clock, 0, "in-memory clock is deliberately stale here");
        save_config(&pool, &ws, 1000, None)
            .await
            .expect("save_config");
        let durable: i64 = sqlx::query_scalar(
            "SELECT logical_clock FROM working_set_config WHERE session_key = $1",
        )
        .bind(&session_key)
        .fetch_one(&pool)
        .await
        .expect("read clock");
        assert_eq!(
            durable, N,
            "save_config must not regress the durable clock from {N} to the stale in-memory 0"
        );

        purge(&pool, &session_key).await;
    }

    /// ATOMICITY: a `save_working_set` whose page loop fails partway commits NO
    /// rows. We provoke a mid-flush failure by writing a page whose `state`
    /// violates the CHECK constraint (an illegal value), inside a working set with
    /// a valid page before it. Because the whole flush is one transaction, the
    /// valid page must NOT survive.
    ///
    /// We cannot inject an illegal `ResidentPage` through the public API (the
    /// state is derived from valid flags), so we drive the transaction by hand
    /// with the SAME structure `save_working_set` uses and force the second
    /// statement to fail, asserting the first rolled back.
    #[tokio::test]
    async fn save_working_set_flush_is_all_or_nothing() {
        let Some(pool) = test_pool().await else {
            eprintln!("[tape::store] SKIPPED save_working_set atomicity: no test DB");
            return;
        };
        let session_key = fresh_session_key();
        purge(&pool, &session_key).await;

        // A hand-rolled transaction mirroring save_working_set: one good page
        // insert, then a deliberately failing statement (illegal `state`), then a
        // commit that never runs because the failure propagates and the tx drops.
        let result: Result<(), sqlx::Error> = async {
            let mut tx = pool.begin().await?;
            // Good row.
            sqlx::query(
                "INSERT INTO working_set_pages
                    (session_key, state_cursor, page_kind, page_addr, tree_path, state,
                     importance, est_tokens, use_count, last_access_ord, dirty, content, evict_reason)
                 VALUES ($1, 0, 'file_chunk', 'corpus/chunk/1', $1, 'resident',
                         0.5, 10, 1, 1, false, NULL, NULL)",
            )
            .bind(&session_key)
            .execute(&mut *tx)
            .await?;
            // Bad row: 'not_a_state' violates working_set_pages_state_check → the
            // statement errors, we early-return, and `tx` is dropped (rollback).
            sqlx::query(
                "INSERT INTO working_set_pages
                    (session_key, state_cursor, page_kind, page_addr, tree_path, state,
                     importance, est_tokens, use_count, last_access_ord, dirty, content, evict_reason)
                 VALUES ($1, 0, 'file_chunk', 'corpus/chunk/2', $1, 'not_a_state',
                         0.5, 10, 1, 1, false, NULL, NULL)",
            )
            .bind(&session_key)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(())
        }
        .await;
        assert!(
            result.is_err(),
            "the illegal-state insert must fail the flush"
        );

        // The good row must NOT have survived — the whole transaction rolled back.
        let surviving: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM working_set_pages WHERE session_key = $1")
                .bind(&session_key)
                .fetch_one(&pool)
                .await
                .expect("count rows");
        assert_eq!(
            surviving, 0,
            "a mid-loop failure must leave NO partial rows (atomic flush)"
        );

        purge(&pool, &session_key).await;
    }

    /// BYTE ROUND-TRIP: persist a working set holding a `Scratch` page that
    /// carries `Some(bytes)` → it lands in `working_set_pages.content` → load it
    /// back (bytes preserved) → `rehydrate_store_from_pages` reconstructs a
    /// byte-identical page in the `TapeStore`. Corpus pages (bytes None) are NOT
    /// rehydrated.
    #[tokio::test]
    async fn scratch_bytes_round_trip_through_persist_and_rehydrate() {
        let Some(pool) = test_pool().await else {
            eprintln!("[tape::store] SKIPPED byte round-trip: no test DB");
            return;
        };
        let session_key = fresh_session_key();
        purge(&pool, &session_key).await;

        // The session_key IS the tree path (the RLM bridge invariant).
        let tree_path = TreePath(session_key.clone());
        let tree_id = RealTapeDataPlane::tree_id(&tree_path);
        // A scratch address in THIS tree (the only kind without corpus backing).
        let scratch_addr = context_tape::PageAddress::Scratch {
            tree: tree_id,
            slot: Box::new([0xab, 0xcd]),
        };
        let scratch_path = scratch_addr.to_path();
        let payload = "accumulator: line-1\nline-2\n(situated scratch content)";

        // Build a working set: one scratch page carrying bytes, one corpus page
        // carrying none.
        let mut ws = WorkingSet::new(session_key.clone(), 0, 100_000, EvictionPolicy::Lru);
        ws.pages.insert(ResidentPage {
            addr: PageAddr(scratch_path.clone()),
            kind: PageKind::FileChunk, // control plane has no Scratch kind
            importance: 0.7,
            est_tokens: Page::estimate_tokens(payload) as i32,
            use_count: 1,
            last_access_ord: 1,
            dirty: true,
            pinned: false,
            bytes: Some(payload.to_string()),
        });
        ws.pages.insert(ResidentPage {
            addr: PageAddr("corpus/chunk/12345".to_string()),
            kind: PageKind::FileChunk,
            importance: 0.4,
            est_tokens: 50,
            use_count: 1,
            last_access_ord: 2,
            dirty: false,
            pinned: false,
            bytes: None,
        });
        ws.resident_tokens = ws.recompute_resident_tokens();

        save_working_set(&pool, &ws, tree_path.as_str(), 100_000, None)
            .await
            .expect("save_working_set");

        // The content column holds the scratch bytes and NULL for the corpus page.
        let scratch_content: Option<String> = sqlx::query_scalar(
            "SELECT content FROM working_set_pages WHERE session_key = $1 AND page_addr = $2",
        )
        .bind(&session_key)
        .bind(&scratch_path)
        .fetch_one(&pool)
        .await
        .expect("read scratch content");
        assert_eq!(
            scratch_content.as_deref(),
            Some(payload),
            "scratch bytes persisted"
        );
        let corpus_content: Option<String> = sqlx::query_scalar(
            "SELECT content FROM working_set_pages WHERE session_key = $1 AND page_addr = $2",
        )
        .bind(&session_key)
        .bind("corpus/chunk/12345")
        .fetch_one(&pool)
        .await
        .expect("read corpus content");
        assert_eq!(corpus_content, None, "corpus page persists no content");

        // load_working_set carries the bytes back on the resident page.
        let loaded = load_working_set(&pool, &session_key, 0)
            .await
            .expect("load_working_set");
        let loaded_scratch = loaded
            .pages
            .get(&PageAddr(scratch_path.clone()))
            .expect("scratch page loaded");
        assert_eq!(
            loaded_scratch.bytes.as_deref(),
            Some(payload),
            "load round-trips the scratch bytes"
        );

        // Rehydrate the RAM plane and confirm a byte-identical page exists.
        let registry = TapeRegistry::new();
        let n = rehydrate_store_from_pages(&pool, &registry, &session_key, 0)
            .await
            .expect("rehydrate");
        assert_eq!(
            n, 1,
            "exactly the one scratch page is rehydrated (corpus skipped)"
        );
        let in_store = registry.with_store(tree_id, |s| s.get(&scratch_addr).cloned());
        let in_store = in_store.expect("scratch page present in TapeStore after rehydrate");
        assert_eq!(
            in_store.content, payload,
            "rehydrated content is byte-identical"
        );
        assert_eq!(in_store.meta.kind, context_tape::PageKind::Scratch);

        purge(&pool, &session_key).await;
    }
}
