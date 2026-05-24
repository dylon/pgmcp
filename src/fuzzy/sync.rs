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

use rmcp::ErrorData as McpError;
use sqlx::PgPool;

use super::persistent_artrie::{FuzzyError, FuzzyIndex};
use super::values::{CommitRef, DurableMandateRef, PathValue, SymbolValue};
use crate::context::SystemContext;
use crate::mcp::tools::sota_helpers::{pool_or_err, project_id_or_err};

/// Rebuild the symbol index from `file_symbols` + `indexed_files`.
/// Returns the number of rows ingested.
pub async fn rebuild_symbols(
    pool: &PgPool,
    project_id: i32,
    idx: &FuzzyIndex<SymbolValue>,
) -> Result<usize, FuzzyError> {
    let rows: Vec<(String, i64, String, String, i32)> =
        sqlx::query_as::<_, (String, i64, String, String, i32)>(
            "SELECT fs.name, fs.file_id, fs.kind, fs.visibility, fs.line_start
             FROM file_symbols fs
             JOIN indexed_files f ON fs.file_id = f.id
             WHERE f.project_id = $1",
        )
        .bind(project_id)
        .fetch_all(pool)
        .await
        .map_err(|e| FuzzyError::Trie(format!("symbol fetch: {e}")))?;

    let mut count = 0usize;
    for (name, file_id, kind, visibility, line) in rows {
        let value = SymbolValue {
            file_id,
            kind,
            visibility,
            line,
        };
        idx.upsert(&name, value)?;
        count += 1;
    }
    Ok(count)
}

/// Rebuild the path index from `indexed_files`.
pub async fn rebuild_paths(
    pool: &PgPool,
    project_id: i32,
    idx: &FuzzyIndex<PathValue>,
) -> Result<usize, FuzzyError> {
    let rows: Vec<(String, i64, i64)> = sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT relative_path, id, COALESCE(size_bytes, 0)
         FROM indexed_files
         WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| FuzzyError::Trie(format!("path fetch: {e}")))?;

    let mut count = 0usize;
    for (relative_path, file_id, size_bytes) in rows {
        let value = PathValue {
            file_id,
            project_id,
            size_bytes,
        };
        idx.upsert(&relative_path, value)?;
        count += 1;
    }
    Ok(count)
}

/// Rebuild the commit-subject index from `git_commits`.
pub async fn rebuild_commits(
    pool: &PgPool,
    project_id: i32,
    idx: &FuzzyIndex<CommitRef>,
) -> Result<usize, FuzzyError> {
    let rows: Vec<(String, i64, String)> = sqlx::query_as::<_, (String, i64, String)>(
        "SELECT subject, id, sha
         FROM git_commits
         WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| FuzzyError::Trie(format!("commit fetch: {e}")))?;

    let mut count = 0usize;
    for (subject, commit_id, sha) in rows {
        let value = CommitRef {
            commit_id,
            project_id,
            sha,
        };
        idx.upsert(&subject, value)?;
        count += 1;
    }
    Ok(count)
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
) -> Result<FuzzyIndex<SymbolValue>, McpError> {
    let data_dir = ctx.config().load().fuzzy.data_dir.clone();
    let slug = crate::cron::fuzzy_sync::slugify(project_name);
    let path = crate::cron::fuzzy_sync::trie_path(&data_dir, "symbols", &slug);
    let fresh = !path.exists();
    let (idx, _recovery) = FuzzyIndex::<SymbolValue>::open_or_create(&path)
        .map_err(|e| McpError::internal_error(format!("fuzzy symbol trie open: {e}"), None))?;
    if fresh {
        let project_id = project_id_or_err(ctx, project_name).await?;
        let pool = pool_or_err(ctx)?;
        rebuild_symbols(pool, project_id, &idx).await.map_err(|e| {
            McpError::internal_error(format!("fuzzy symbol trie initial warm: {e}"), None)
        })?;
        tracing::info!(
            path = %path.display(),
            project = %project_name,
            entries = idx.len(),
            "fuzzy symbol trie lazy-warmed from PG"
        );
    }
    Ok(idx)
}

/// Open (or create + lazy-warm from PG) the per-project path
/// `FuzzyIndex`. Mirror of `open_symbol_trie` for
/// `indexed_files.relative_path` keyed lookups.
pub async fn open_path_trie(
    ctx: &SystemContext,
    project_name: &str,
) -> Result<FuzzyIndex<PathValue>, McpError> {
    let data_dir = ctx.config().load().fuzzy.data_dir.clone();
    let slug = crate::cron::fuzzy_sync::slugify(project_name);
    let path = crate::cron::fuzzy_sync::trie_path(&data_dir, "paths", &slug);
    let fresh = !path.exists();
    let (idx, _recovery) = FuzzyIndex::<PathValue>::open_or_create(&path)
        .map_err(|e| McpError::internal_error(format!("fuzzy path trie open: {e}"), None))?;
    if fresh {
        let project_id = project_id_or_err(ctx, project_name).await?;
        let pool = pool_or_err(ctx)?;
        rebuild_paths(pool, project_id, &idx).await.map_err(|e| {
            McpError::internal_error(format!("fuzzy path trie initial warm: {e}"), None)
        })?;
        tracing::info!(
            path = %path.display(),
            project = %project_name,
            entries = idx.len(),
            "fuzzy path trie lazy-warmed from PG"
        );
    }
    Ok(idx)
}

/// Rebuild the durable-mandate index from `durable_mandates`.
pub async fn rebuild_durable_mandates(
    pool: &PgPool,
    idx: &FuzzyIndex<DurableMandateRef>,
) -> Result<usize, FuzzyError> {
    let rows: Vec<(String, i64, String, String)> =
        sqlx::query_as::<_, (String, i64, String, String)>(
            "SELECT imperative, id, scope, polarity FROM durable_mandates",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| FuzzyError::Trie(format!("durable mandate fetch: {e}")))?;

    let mut count = 0usize;
    for (imperative, mandate_id, scope, polarity) in rows {
        let value = DurableMandateRef {
            mandate_id,
            scope,
            polarity,
        };
        idx.upsert(&imperative, value)?;
        count += 1;
    }
    Ok(count)
}
