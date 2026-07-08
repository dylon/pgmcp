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
        let every = enable_eviction_for_warm(ctx, &idx);
        let pool = pool_or_err(ctx)?;
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
        let every = enable_eviction_for_warm(ctx, &idx);
        let pool = pool_or_err(ctx)?;
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
        if count % every == 0 {
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
        if count % every == 0 {
            idx.checkpoint()?;
        }
    }
    Ok(count)
}
