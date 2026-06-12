//! Freshness gate + index reconciliation + optional topic enrichment.
//!
//! This is the mechanism behind "use the temporal graph as an authority only
//! when it is reliable": for each project we compare the live working-tree HEAD
//! against the indexed `git_last_commit` watermark, and the live commit count in
//! the window against the indexed `git_commits` count. Enrichment (indexed topic
//! labels) is attached only when the project is `fresh`. The live-git numbers are
//! always authoritative; enrichment never overrides them.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::db::queries::get_git_last_commit;

#[derive(Debug, Clone, serde::Serialize)]
pub struct EnrichmentInfo {
    /// `fresh` (indexed HEAD == live HEAD), `stale` (indexed but behind), or
    /// `unindexed` (no git-history index for this project).
    pub freshness: String,
    pub indexed_commits_in_window: Option<i64>,
    pub live_commits_in_window: u64,
    /// `match` | `index_behind` | `index_ahead` | `n/a`.
    pub reconciliation: String,
    /// Indexed topic labels touching this project (only when `fresh` + graph on).
    pub topics: Vec<String>,
}

/// Compute the enrichment/freshness verdict for one project. Best-effort: any DB
/// error degrades to `unindexed` / empty rather than failing the summary.
#[allow(clippy::too_many_arguments)]
pub async fn enrich(
    pool: &PgPool,
    project_id: i32,
    repo_path: &str,
    project_name: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    author: Option<&str>,
    live_commits: u64,
    use_graph: bool,
) -> EnrichmentInfo {
    let live_head = crate::deps::gitstate::read_git_state(repo_path).head_sha;
    let indexed_head = get_git_last_commit(pool, project_id).await.ok().flatten();

    let freshness = match (&indexed_head, &live_head) {
        (None, _) => "unindexed",
        (Some(i), Some(l)) if i == l => "fresh",
        (Some(_), _) => "stale",
    };

    // Indexed commit count in the same window/author, for reconciliation.
    let indexed_count: Option<i64> = if freshness == "unindexed" {
        None
    } else {
        let q = "SELECT COUNT(*) FROM git_commits \
                 WHERE project_id = $1 AND author_date >= $2 AND author_date < $3 \
                 AND ($4::text IS NULL OR author ~* $4)";
        sqlx::query_scalar::<_, i64>(q)
            .bind(project_id)
            .bind(since)
            .bind(until)
            .bind(author)
            .fetch_one(pool)
            .await
            .ok()
    };

    let reconciliation = match indexed_count {
        None => "n/a",
        Some(c) if c as u64 == live_commits => "match",
        Some(c) if (c as u64) < live_commits => "index_behind",
        Some(_) => "index_ahead",
    };

    let topics = if use_graph && freshness == "fresh" {
        sqlx::query_scalar::<_, String>(
            "SELECT label FROM code_topics \
             WHERE $1 = ANY(project_names) ORDER BY chunk_count DESC LIMIT 5",
        )
        .bind(project_name)
        .fetch_all(pool)
        .await
        .unwrap_or_default()
    } else {
        Vec::new()
    };

    EnrichmentInfo {
        freshness: freshness.to_string(),
        indexed_commits_in_window: indexed_count,
        live_commits_in_window: live_commits,
        reconciliation: reconciliation.to_string(),
        topics,
    }
}
