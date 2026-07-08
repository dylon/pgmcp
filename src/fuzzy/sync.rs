//! PG → trie synchronization.
//!
//! Each `FuzzyIndex` is rebuildable from its canonical PG table; this
//! module owns the SELECT loops that materialize the trie at daemon
//! start and refresh it on the `fuzzy_sync` cron schedule.
//!
//! Also exposes `open_symbol_trie` / `open_path_trie` — the
//! tool-facing helpers that open the per-project persistent
//! `FuzzyIndex` and lazy-warm it from PG on first call.
//! `tool_fuzzy_symbol_search`, `tool_fuzzy_path_search`, and
//! `tool_hybrid_search::try_third_leg` all consume these to avoid
//! rebuilding a transient `DynamicDawgChar` per call.

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use sqlx::PgPool;

use super::persistent_artrie::{FuzzyError, FuzzyIndex};
use super::values::{CommitRef, ConceptValue, DurableMandateRef, PathValue, SymbolValue};
use crate::context::SystemContext;
use crate::mcp::tools::sota_helpers::{pool_or_err, project_id_or_err};

/// Which per-project fuzzy source a rebuild reads — selects the row-count
/// subquery used by [`source_exceeds`] for the skip-oversize guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuzzySource {
    /// `file_symbols ⨝ indexed_files` (source of the symbols trie).
    Symbols,
    /// `indexed_files` (source of the paths trie).
    Paths,
    /// `git_commits` (source of the commits trie).
    Commits,
}

/// BOUNDED-count guard for the skip-oversize policy (the reliable, active
/// 2026-07-08 `fuzzy-sync` OOM fix).
///
/// Returns `true` iff the project's source-row count for `source` **exceeds**
/// `threshold`. Efficient by construction: the inner `SELECT 1 … LIMIT $1`
/// (with `$1 = threshold + 1`) makes Postgres stop scanning after
/// `threshold + 1` rows, so this NEVER counts all ~22 M rows of a pathological
/// project — it answers the yes/no question in `O(threshold)` at most. The
/// project filter binds `$2`.
///
/// `threshold == 0` disables the guard (returns `false` without querying),
/// mirroring `[fuzzy] oversize_trie_row_threshold = 0` ("never skip").
pub async fn source_exceeds(
    pool: &PgPool,
    project_id: i32,
    source: FuzzySource,
    threshold: u64,
) -> Result<bool, FuzzyError> {
    if threshold == 0 {
        return Ok(false);
    }
    let limit = oversize_probe_limit(threshold);
    let query = match source {
        FuzzySource::Symbols => sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM (SELECT 1 FROM file_symbols fs \
             JOIN indexed_files f ON fs.file_id = f.id \
             WHERE f.project_id = $2 LIMIT $1) t",
        ),
        FuzzySource::Paths => sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM (SELECT 1 FROM indexed_files \
             WHERE project_id = $2 LIMIT $1) t",
        ),
        FuzzySource::Commits => sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM (SELECT 1 FROM git_commits \
             WHERE project_id = $2 LIMIT $1) t",
        ),
    };
    let count: i64 = query
        .bind(limit)
        .bind(project_id)
        .fetch_one(pool)
        .await
        .map_err(|e| FuzzyError::Trie(format!("source_exceeds count: {e}")))?;
    Ok(source_is_over(count, threshold))
}

/// The `LIMIT` bind for the bounded-count probe: `threshold + 1`, saturating and
/// clamped to a valid non-negative `i64` even at the `u64` ceiling. Pure so the
/// skip-oversize arithmetic is unit-tested without a DB.
fn oversize_probe_limit(threshold: u64) -> i64 {
    threshold.saturating_add(1).min(i64::MAX as u64) as i64
}

/// Pure skip decision: the bounded count (capped at `threshold + 1` by the
/// `LIMIT`) exceeds `threshold`. A negative count (impossible from `count(*)`)
/// is treated as not-over.
fn source_is_over(count_capped: i64, threshold: u64) -> bool {
    count_capped >= 0 && count_capped as u64 > threshold
}

/// Rebuild the symbol index from `file_symbols` + `indexed_files`.
/// Returns the number of rows ingested.
///
/// **Memory-bounded (2026-07-08 OOM fix).** The source is keyset-paginated by
/// `fs.id` in pages of `checkpoint_every_rows`, and the overlay is checkpointed
/// after each page — so neither the source result set nor the in-memory overlay is
/// ever materialized whole. With eviction enabled + a `resident_budget_bytes`
/// (the caller enables it BEFORE calling this), each checkpoint swizzles the
/// coldest overlay nodes to disk, keeping rebuild RAM ≈ one page + the budget
/// regardless of how many symbols the project has (the `default` project had 22 M,
/// an 11.5 GB trie built entirely in RAM under the old whole-`fetch_all` path).
pub async fn rebuild_symbols(
    pool: &PgPool,
    project_id: i32,
    idx: &FuzzyIndex<SymbolValue>,
    checkpoint_every_rows: usize,
) -> Result<usize, FuzzyError> {
    let page = checkpoint_every_rows.max(1) as i64;
    let mut last_id = 0i64;
    let mut count = 0usize;
    loop {
        let rows: Vec<(String, i64, String, String, i32, i64)> =
            sqlx::query_as::<_, (String, i64, String, String, i32, i64)>(
                // `file_symbols.visibility` is nullable (the symbol extractor leaves
                // it NULL for symbols whose visibility it can't determine). COALESCE
                // to the project-wide convention 'private' so the non-Option tuple
                // slot never decodes a NULL (see tool_dead_code_reachability). `fs.id`
                // is the unique keyset cursor (one row per symbol).
                "SELECT fs.name, fs.file_id, fs.kind, COALESCE(fs.visibility, 'private'), fs.start_line, fs.id
                 FROM file_symbols fs
                 JOIN indexed_files f ON fs.file_id = f.id
                 WHERE f.project_id = $1 AND fs.id > $2
                 ORDER BY fs.id
                 LIMIT $3",
            )
            .bind(project_id)
            .bind(last_id)
            .bind(page)
            .fetch_all(pool)
            .await
            .map_err(|e| FuzzyError::Trie(format!("symbol fetch: {e}")))?;
        if rows.is_empty() {
            break;
        }
        for (name, file_id, kind, visibility, line, id) in rows {
            idx.upsert(
                &name,
                SymbolValue {
                    file_id,
                    kind,
                    visibility,
                    line,
                },
            )?;
            last_id = id;
            count += 1;
        }
        idx.checkpoint()?;
    }
    Ok(count)
}

/// Rebuild the path index from `indexed_files`. Memory-bounded like
/// [`rebuild_symbols`] (keyset-paginated by `id`, checkpointed per page).
pub async fn rebuild_paths(
    pool: &PgPool,
    project_id: i32,
    idx: &FuzzyIndex<PathValue>,
    checkpoint_every_rows: usize,
) -> Result<usize, FuzzyError> {
    let page = checkpoint_every_rows.max(1) as i64;
    let mut last_id = 0i64;
    let mut count = 0usize;
    loop {
        let rows: Vec<(String, i64, i64)> = sqlx::query_as::<_, (String, i64, i64)>(
            "SELECT relative_path, id, COALESCE(size_bytes, 0)
             FROM indexed_files
             WHERE project_id = $1 AND id > $2
             ORDER BY id
             LIMIT $3",
        )
        .bind(project_id)
        .bind(last_id)
        .bind(page)
        .fetch_all(pool)
        .await
        .map_err(|e| FuzzyError::Trie(format!("path fetch: {e}")))?;
        if rows.is_empty() {
            break;
        }
        for (relative_path, file_id, size_bytes) in rows {
            idx.upsert(
                &relative_path,
                PathValue {
                    file_id,
                    project_id,
                    size_bytes,
                },
            )?;
            last_id = file_id;
            count += 1;
        }
        idx.checkpoint()?;
    }
    Ok(count)
}

/// Rebuild the commit-subject index from `git_commits`. Memory-bounded like
/// [`rebuild_symbols`] (keyset-paginated by `id`, checkpointed per page).
pub async fn rebuild_commits(
    pool: &PgPool,
    project_id: i32,
    idx: &FuzzyIndex<CommitRef>,
    checkpoint_every_rows: usize,
) -> Result<usize, FuzzyError> {
    let page = checkpoint_every_rows.max(1) as i64;
    let mut last_id = 0i64;
    let mut count = 0usize;
    loop {
        let rows: Vec<(String, i64, String)> = sqlx::query_as::<_, (String, i64, String)>(
            // The commit-hash column is `commit_hash`, not `sha` (the old name
            // referenced a nonexistent column and failed at plan time).
            "SELECT subject, id, commit_hash
             FROM git_commits
             WHERE project_id = $1 AND id > $2
             ORDER BY id
             LIMIT $3",
        )
        .bind(project_id)
        .bind(last_id)
        .bind(page)
        .fetch_all(pool)
        .await
        .map_err(|e| FuzzyError::Trie(format!("commit fetch: {e}")))?;
        if rows.is_empty() {
            break;
        }
        for (subject, commit_id, sha) in rows {
            idx.upsert(
                &subject,
                CommitRef {
                    commit_id,
                    project_id,
                    sha,
                },
            )?;
            last_id = commit_id;
            count += 1;
        }
        idx.checkpoint()?;
    }
    Ok(count)
}

/// Lazy-warm prologue shared by the tool-facing `open_*_trie` helpers: enable heap
/// eviction on a freshly-created trie (when `[fuzzy] max_disk_bytes > 0`) so an
/// initial PG warm of a large project is memory-bounded — the resident-budget
/// eviction tail runs on each rebuild checkpoint, keeping the overlay ≈ one page +
/// the budget instead of building the whole trie in RAM. Returns the configured
/// checkpoint stride to hand to the `rebuild_*` call.
fn enable_eviction_for_warm<V>(ctx: &SystemContext, idx: &FuzzyIndex<V>) -> usize
where
    V: libdictenstein::DictionaryValue + Clone + Send + Sync + 'static,
{
    let cfg = ctx.config().load();
    if cfg.fuzzy.max_disk_bytes > 0 {
        // Tolerate "already enabled" (a reused handle) like finalize_trie does.
        let _ = idx.enable_eviction(cfg.fuzzy.eviction_config());
    }
    cfg.fuzzy.checkpoint_every_rows
}

/// Open (or create + lazy-warm from PG) the per-project symbol
/// `FuzzyIndex`. Used by `tool_fuzzy_symbol_search` and
/// `tool_hybrid_search::try_third_leg` to consume the persistent
/// `PersistentARTrieChar`-backed trie that `cron::fuzzy_sync`
/// materializes periodically. Lazy warming runs only when the trie
/// file does not yet exist (fresh deployment); thereafter the cron
/// keeps the trie current and this helper just mmap-attaches.
pub async fn open_symbol_trie(
    ctx: &SystemContext,
    project_name: &str,
) -> Result<Arc<FuzzyIndex<SymbolValue>>, McpError> {
    let project_name = project_name.trim();
    let project_id = project_id_or_err(ctx, project_name).await?;
    let data_dir = ctx.config().load().fuzzy.data_dir.clone();
    let key = crate::cron::fuzzy_sync::project_artifact_key(project_id, project_name);
    let path = crate::cron::fuzzy_sync::trie_path(&data_dir, "symbols", &key);

    // Reuse a cached handle if the on-disk trie is unchanged since it was opened
    // (the cron bumps the file's mtime when it rebuilds). Opening a handle spawns
    // three daemon threads, so reuse avoids per-call thread churn.
    if let Some(idx) = ctx.fuzzy_cache().get_symbols(&key, &path) {
        return Ok(idx);
    }

    let fresh = !path.exists();
    let (idx, _recovery) = FuzzyIndex::<SymbolValue>::open_or_create(&path)
        .map_err(|e| McpError::internal_error(format!("fuzzy symbol trie open: {e}"), None))?;
    if fresh {
        let threshold = ctx.config().load().fuzzy.oversize_trie_row_threshold;
        let pool = pool_or_err(ctx)?;
        // Skip-oversize guard (mirror of the cron path): never build a
        // pathologically large trie in RAM on first-touch lazy warm. On skip we
        // still return the freshly-opened (empty) trie — it serves no fuzzy hits
        // until the source drops under the cap, but it never OOMs the daemon.
        if source_exceeds(pool, project_id, FuzzySource::Symbols, threshold)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("fuzzy symbol oversize check: {e}"), None)
            })?
        {
            tracing::warn!(
                path = %path.display(),
                project = %project_name,
                threshold,
                "skipping oversize fuzzy symbol trie lazy-warm (source rows exceed \
                 [fuzzy] oversize_trie_row_threshold); serving empty trie until bounded"
            );
        } else {
            let every = enable_eviction_for_warm(ctx, &idx);
            rebuild_symbols(pool, project_id, &idx, every)
                .await
                .map_err(|e| {
                    McpError::internal_error(format!("fuzzy symbol trie initial warm: {e}"), None)
                })?;
            tracing::info!(
                path = %path.display(),
                project = %project_name,
                entries = idx.len(),
                "fuzzy symbol trie lazy-warmed from PG"
            );
        }
    }
    Ok(ctx.fuzzy_cache().insert_symbols(&key, &path, idx))
}

/// Open (or create + lazy-warm from PG) the per-project path
/// `FuzzyIndex`. Mirror of `open_symbol_trie` for
/// `indexed_files.relative_path` keyed lookups.
pub async fn open_path_trie(
    ctx: &SystemContext,
    project_name: &str,
) -> Result<Arc<FuzzyIndex<PathValue>>, McpError> {
    let project_name = project_name.trim();
    let project_id = project_id_or_err(ctx, project_name).await?;
    let data_dir = ctx.config().load().fuzzy.data_dir.clone();
    let key = crate::cron::fuzzy_sync::project_artifact_key(project_id, project_name);
    let path = crate::cron::fuzzy_sync::trie_path(&data_dir, "paths", &key);

    if let Some(idx) = ctx.fuzzy_cache().get_paths(&key, &path) {
        return Ok(idx);
    }

    let fresh = !path.exists();
    let (idx, _recovery) = FuzzyIndex::<PathValue>::open_or_create(&path)
        .map_err(|e| McpError::internal_error(format!("fuzzy path trie open: {e}"), None))?;
    if fresh {
        let threshold = ctx.config().load().fuzzy.oversize_trie_row_threshold;
        let pool = pool_or_err(ctx)?;
        // Skip-oversize guard (mirror of the cron path); see `open_symbol_trie`.
        if source_exceeds(pool, project_id, FuzzySource::Paths, threshold)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("fuzzy path oversize check: {e}"), None)
            })?
        {
            tracing::warn!(
                path = %path.display(),
                project = %project_name,
                threshold,
                "skipping oversize fuzzy path trie lazy-warm (source rows exceed \
                 [fuzzy] oversize_trie_row_threshold); serving empty trie until bounded"
            );
        } else {
            let every = enable_eviction_for_warm(ctx, &idx);
            rebuild_paths(pool, project_id, &idx, every)
                .await
                .map_err(|e| {
                    McpError::internal_error(format!("fuzzy path trie initial warm: {e}"), None)
                })?;
            tracing::info!(
                path = %path.display(),
                project = %project_name,
                entries = idx.len(),
                "fuzzy path trie lazy-warmed from PG"
            );
        }
    }
    Ok(ctx.fuzzy_cache().insert_paths(&key, &path, idx))
}

/// Rebuild the workspace-global concept index from
/// `ontology_concept_meta ⨝ memory_entities` — one entry per concept *name*
/// across all projects (workspace rollups carry `project_id = NULL`). The trie
/// is a **fuzzy name-matcher**: `ontology_search` resolves matched names back
/// through PG (`WHERE name = ANY(...) AND valid_to IS NULL`), so same-name
/// concepts in different projects and stale/deleted names never yield an
/// incorrect row — the trie only proposes candidates, PG remains authoritative.
pub async fn rebuild_concepts(
    pool: &PgPool,
    idx: &FuzzyIndex<ConceptValue>,
    checkpoint_every_rows: usize,
) -> Result<usize, FuzzyError> {
    // Workspace-global + modest in size, so the source set is loaded whole; the
    // overlay is still checkpointed every `checkpoint_every_rows` inserts so it is
    // bounded (with eviction enabled by the caller) even if the ontology grows.
    let rows: Vec<(String, i64, String, String, Option<i32>)> =
        sqlx::query_as::<_, (String, i64, String, String, Option<i32>)>(
            "SELECT e.name, e.id, m.facet, m.status, m.project_id
             FROM ontology_concept_meta m
             JOIN memory_entities e ON e.id = m.entity_id AND e.valid_to IS NULL",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| FuzzyError::Trie(format!("concept fetch: {e}")))?;

    let every = checkpoint_every_rows.max(1);
    let mut count = 0usize;
    for (name, entity_id, facet, status, project_id) in rows {
        idx.upsert(
            &name,
            ConceptValue {
                entity_id,
                facet,
                status,
                project_id,
            },
        )?;
        count += 1;
        if count.is_multiple_of(every) {
            idx.checkpoint()?;
        }
    }
    Ok(count)
}

/// Open (or create + lazy-warm from PG) the **workspace-global** concept
/// `FuzzyIndex`. Backs the typo-tolerant / prefix legs of `ontology_search`
/// and the `{concept}` resource-template completion. Unlike the per-project
/// symbol/path tries this is a single global trie (concepts span projects and
/// include workspace-level rollups); `ConceptValue` carries `project_id` so a
/// caller can still filter by project in-trie. Lazy warming runs only on first
/// creation; thereafter the `fuzzy-sync` cron keeps it current.
pub async fn open_concept_trie(
    ctx: &SystemContext,
) -> Result<Arc<FuzzyIndex<ConceptValue>>, McpError> {
    let data_dir = ctx.config().load().fuzzy.data_dir.clone();
    let slug = crate::cron::fuzzy_sync::CONCEPT_TRIE_SLUG;
    let path = crate::cron::fuzzy_sync::concept_trie_path(&data_dir);

    if let Some(idx) = ctx.fuzzy_cache().get_concepts(slug, &path) {
        return Ok(idx);
    }

    let fresh = !path.exists();
    let (idx, _recovery) = FuzzyIndex::<ConceptValue>::open_or_create(&path)
        .map_err(|e| McpError::internal_error(format!("fuzzy concept trie open: {e}"), None))?;
    if fresh {
        let every = enable_eviction_for_warm(ctx, &idx);
        let pool = pool_or_err(ctx)?;
        rebuild_concepts(pool, &idx, every).await.map_err(|e| {
            McpError::internal_error(format!("fuzzy concept trie initial warm: {e}"), None)
        })?;
        tracing::info!(
            path = %path.display(),
            entries = idx.len(),
            "fuzzy concept trie lazy-warmed from PG"
        );
    }
    Ok(ctx.fuzzy_cache().insert_concepts(slug, &path, idx))
}

/// Rebuild the durable-mandate index from `durable_mandates`.
pub async fn rebuild_durable_mandates(
    pool: &PgPool,
    idx: &FuzzyIndex<DurableMandateRef>,
    checkpoint_every_rows: usize,
) -> Result<usize, FuzzyError> {
    let rows: Vec<(String, i64, String, String)> =
        sqlx::query_as::<_, (String, i64, String, String)>(
            "SELECT imperative, id, scope, polarity FROM durable_mandates",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| FuzzyError::Trie(format!("durable mandate fetch: {e}")))?;

    let every = checkpoint_every_rows.max(1);
    let mut count = 0usize;
    for (imperative, mandate_id, scope, polarity) in rows {
        idx.upsert(
            &imperative,
            DurableMandateRef {
                mandate_id,
                scope,
                polarity,
            },
        )?;
        count += 1;
        if count.is_multiple_of(every) {
            idx.checkpoint()?;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversize_probe_limit_is_threshold_plus_one_and_saturates() {
        assert_eq!(oversize_probe_limit(0), 1, "threshold 0 → LIMIT 1");
        assert_eq!(oversize_probe_limit(3_000_000), 3_000_001);
        // At the u64 ceiling the +1 saturates and the value is clamped into i64.
        assert_eq!(oversize_probe_limit(u64::MAX), i64::MAX);
        assert_eq!(oversize_probe_limit(i64::MAX as u64), i64::MAX);
    }

    #[test]
    fn source_is_over_only_when_bounded_count_exceeds_threshold() {
        // Bounded count comes back capped at threshold + 1.
        assert!(
            source_is_over(3_000_001, 3_000_000),
            "capped count threshold+1 ⇒ over"
        );
        assert!(
            !source_is_over(3_000_000, 3_000_000),
            "exactly at threshold ⇒ not over"
        );
        assert!(!source_is_over(0, 3_000_000), "empty source ⇒ not over");
        assert!(!source_is_over(-1, 3_000_000), "negative count ⇒ not over");
    }
}
