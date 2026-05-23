//! PG → trie synchronization.
//!
//! Each `FuzzyIndex` is rebuildable from its canonical PG table; this
//! module owns the SELECT loops that materialize the trie at daemon
//! start and refresh it on the `fuzzy_sync` cron schedule.

use sqlx::PgPool;

use super::persistent_artrie::{FuzzyError, FuzzyIndex};
use super::values::{CommitRef, DurableMandateRef, PathValue, SymbolValue};

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
