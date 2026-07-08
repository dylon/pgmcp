//! Independent, finding-focused collectors over the indexed tables.
//!
//! Each `collect_<name>` queries the same underlying tables a corresponding MCP
//! analysis tool reads, but returns the unified [`Finding`] vocabulary directly
//! — uncapped and typed (no JSON round-trip, no stats-counter side effects, no
//! refactor of the working tools). The aggregator fans these out and the
//! pillars/findings are assembled from their output.
//!
//! Severity is synthesized per-collector following the calibration table in the
//! plan: tools that already tier their output pass it through; the rest map raw
//! scores to fixed cutoffs (not quantiles, which would flap between runs).
#![allow(dead_code)]

pub mod architecture;
pub mod code_health;
pub mod concurrency;
pub mod dependency;
pub mod duplication;
pub mod hygiene;
pub mod security;
pub mod tests_docs;

use rmcp::ErrorData as McpError;
use sqlx::postgres::PgPool;

/// Marker regex for documented-tech-debt scans (shared by a couple collectors).
pub(crate) const DEBT_MARKER_PATTERN: &str = r"(?i)\b(TODO|FIXME|HACK|XXX|TEMP|WORKAROUND|BUG)\b";

/// Truncate a string to `max` chars with an ellipsis, for finding previews.
pub(crate) fn truncate_preview(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// Load `(relative_path, content)` for every text file in a project, with the
/// per-statement timeout lifted for the corpus-scale content scan. A full
/// `indexed_files` content read on a large project routinely exceeds the pool's
/// 30 s default and would otherwise cancel a collector mid-scan (SQLSTATE 57014);
/// [`crate::db::pool::begin_heavy`] lifts the ceiling to 600 s inside a committed
/// transaction. `path_regex`, when `Some`, restricts to paths matching a
/// PostgreSQL POSIX regex (`~`). `content` is `Option<String>` because the column
/// is nullable, though the `content IS NOT NULL` predicate guarantees every
/// returned row carries `Some`.
///
/// Shared by every content-scanning quality collector so the SQL and the timeout
/// lift live in exactly one place.
pub(crate) async fn load_project_file_contents(
    pool: &PgPool,
    project_id: i32,
    path_regex: Option<&str>,
) -> Result<Vec<(String, Option<String>)>, McpError> {
    let mut tx = crate::db::pool::begin_heavy(pool, "600s", "quality-history")
        .await
        .map_err(|e| McpError::internal_error(format!("content fetch begin failed: {e}"), None))?;
    let rows = match path_regex {
        Some(rx) => {
            sqlx::query_as::<_, (String, Option<String>)>(
                "SELECT relative_path, content FROM indexed_files
                 WHERE project_id = $1 AND content IS NOT NULL AND relative_path ~ $2",
            )
            .bind(project_id)
            .bind(rx)
            .fetch_all(&mut *tx)
            .await
        }
        None => {
            sqlx::query_as::<_, (String, Option<String>)>(
                "SELECT relative_path, content FROM indexed_files
                 WHERE project_id = $1 AND content IS NOT NULL",
            )
            .bind(project_id)
            .fetch_all(&mut *tx)
            .await
        }
    }
    .map_err(|e| McpError::internal_error(format!("content fetch failed: {e}"), None))?;
    tx.commit()
        .await
        .map_err(|e| McpError::internal_error(format!("content fetch commit failed: {e}"), None))?;
    Ok(rows)
}
