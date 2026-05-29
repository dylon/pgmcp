//! `status_snapshot` and its row structs (edge-type/topic-scope/per-project/
//! git-project stats). Extracted from `queries.rs` (god-file split).
#![allow(unused_imports)]

use crate::db::queries::*;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

// ============================================================================
// Status snapshot — consumed by `pgmcp status` CLI and `/api/status` REST
// ============================================================================

/// Per-table freshness rollup for the `pgmcp status` output. Every field
/// is derived from a single batched SQL round-trip in
/// [`status_snapshot`]. Counts are cheap (`COUNT(*)`) — for tables with
/// millions of rows they're still served by PG's planner against
/// pre-computed pg_class.reltuples in practice. The exact counts here
/// are observability data, not authoritative.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StatusSnapshot {
    pub project_count: i64,
    pub indexed_file_count: i64,
    pub chunk_count: i64,
    pub git_commit_count: i64,
    pub git_commit_chunk_count: i64,

    pub topic_count_global: i64,
    pub topic_count_total: i64,
    pub topic_assignments_total: i64,
    pub topic_last_computed: Option<DateTime<Utc>>,
    pub topic_noise_chunk_count: i64,
    pub topic_breakdown_by_scope: Vec<TopicScopeStat>,

    pub similarity_pair_count: i64,
    pub similarity_distinct_files: i64,
    pub similarity_last_computed: Option<DateTime<Utc>>,

    pub file_metric_count: i64,
    pub graph_edge_count: i64,
    pub graph_edges_by_type: Vec<EdgeTypeCount>,
    pub graph_metric_last_computed: Option<DateTime<Utc>>,
    pub graph_edge_last_computed: Option<DateTime<Utc>>,

    pub blame_coverage_with: i64,
    pub blame_coverage_total: i64,

    pub per_project: Vec<PerProjectStat>,
    pub git_per_project: Vec<GitProjectStat>,

    pub last_indexed_at: Option<DateTime<Utc>>,
    pub server_version: Option<String>,
    pub vector_extension_version: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EdgeTypeCount {
    pub edge_type: String,
    pub count: i64,
}

/// Per-(scope, topic) summary used by `pgmcp status topics`. `scope`
/// is `'*'` for the global cron-driven scan; per-project scopes look
/// like `'project:<name>'`. `last_computed` is the MAX(`computed_at`)
/// across all topics in that scope.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TopicScopeStat {
    pub scope: String,
    pub topic_count: i64,
    pub last_computed: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PerProjectStat {
    pub project_name: String,
    pub indexed_file_count: i64,
    pub chunk_count: i64,
    pub file_metric_count: i64,
    pub last_indexed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GitProjectStat {
    pub project_name: String,
    pub commit_count: i64,
    pub last_commit_hash: Option<String>,
    pub last_commit_date: Option<DateTime<Utc>>,
}

/// Tuple shape of the per-project rollup query.
type PerProjectRow = (String, i64, i64, i64, Option<DateTime<Utc>>);
/// Tuple shape of the per-project git rollup query.
type GitProjectRow = (String, i64, Option<String>, Option<DateTime<Utc>>);

/// Read every counter + timestamp the status command needs, in one
/// transaction. Each query is `COUNT(*)` or `MAX(timestamp)` — cheap
/// enough that a single status call is fine to issue against a busy
/// daemon.
pub async fn status_snapshot(pool: &PgPool) -> Result<StatusSnapshot, sqlx::Error> {
    // Counts (every table has its own access pattern; do them in one
    // sequential transaction so a single connection serves the whole
    // request — no pool churn).
    let mut tx = pool.begin().await?;

    let project_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM projects")
        .fetch_one(&mut *tx)
        .await?;
    let indexed_file_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM indexed_files")
        .fetch_one(&mut *tx)
        .await?;
    let chunk_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_chunks")
        .fetch_one(&mut *tx)
        .await?;

    // git tables only exist if the migrations created them — but they
    // always do in pgmcp, so unconditional COUNT is safe.
    let git_commit_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM git_commits")
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(0);
    let git_commit_chunk_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM git_commit_chunks")
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(0);

    // `scope = 'global'` is what `cron::topic_clustering::run_global_topic_scan`
    // writes (NOT `'*'` — that string is the MCP tool API "match-all"
    // *parameter*, not a stored scope value).
    let topic_count_global: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM code_topics WHERE scope = 'global'")
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(0);
    let topic_count_total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM code_topics")
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(0);
    let topic_assignments_total: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM chunk_topic_assignments")
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(0);
    let topic_last_computed: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT MAX(computed_at) FROM code_topics")
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(None);

    // Noise = chunks with NO entry in chunk_topic_assignments. Only
    // meaningful once topics have been computed (otherwise everything
    // is "noise" trivially); the CLI labels the field accordingly.
    let topic_noise_chunk_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM file_chunks c \
         WHERE NOT EXISTS (SELECT 1 FROM chunk_topic_assignments a WHERE a.chunk_id = c.id)",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap_or(0);

    // Per-scope topic breakdown (one row per distinct scope).
    let topic_scope_rows: Vec<(String, i64, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT scope, COUNT(*)::BIGINT, MAX(computed_at) \
         FROM code_topics GROUP BY scope ORDER BY scope",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap_or_default();
    let topic_breakdown_by_scope: Vec<TopicScopeStat> = topic_scope_rows
        .into_iter()
        .map(|(scope, topic_count, last_computed)| TopicScopeStat {
            scope,
            topic_count,
            last_computed,
        })
        .collect();

    let similarity_pair_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM cross_project_similarities")
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(0);
    let similarity_distinct_files: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
             SELECT file_id_a AS f FROM cross_project_similarities
             UNION
             SELECT file_id_b AS f FROM cross_project_similarities
         ) AS u",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap_or(0);
    let similarity_last_computed: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT MAX(computed_at) FROM cross_project_similarities")
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(None);

    let file_metric_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_metrics")
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(0);
    let graph_edge_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM code_graph_edges")
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(0);
    let graph_edge_rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT edge_type, COUNT(*)::BIGINT FROM code_graph_edges \
         GROUP BY edge_type ORDER BY edge_type",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap_or_default();
    let graph_edges_by_type: Vec<EdgeTypeCount> = graph_edge_rows
        .into_iter()
        .map(|(edge_type, count)| EdgeTypeCount { edge_type, count })
        .collect();
    let graph_metric_last_computed: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT MAX(computed_at) FROM file_metrics")
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(None);
    let graph_edge_last_computed: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT MAX(computed_at) FROM code_graph_edges")
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(None);

    // Blame coverage on file_chunks. blame_commit is added by an
    // ALTER in migrations and is NULL until the git-history-index
    // cron has populated it.
    let blame_coverage_with: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM file_chunks WHERE blame_commit IS NOT NULL")
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(0);
    let blame_coverage_total: i64 = chunk_count;

    // Per-project breakdown: indexed_files, chunks, file_metrics,
    // last_indexed for every project. Single LEFT-JOIN'd query.
    let per_project_rows: Vec<PerProjectRow> = sqlx::query_as(
        "SELECT
             p.name,
             COUNT(DISTINCT f.id)::BIGINT,
             COUNT(c.id)::BIGINT,
             COUNT(DISTINCT fm.file_id)::BIGINT,
             MAX(f.modified_at)
         FROM projects p
         LEFT JOIN indexed_files f ON f.project_id = p.id
         LEFT JOIN file_chunks c ON c.file_id = f.id
         LEFT JOIN file_metrics fm ON fm.file_id = f.id
         GROUP BY p.id, p.name
         ORDER BY p.name",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap_or_default();
    let per_project: Vec<PerProjectStat> = per_project_rows
        .into_iter()
        .map(
            |(
                project_name,
                indexed_file_count,
                chunk_count,
                file_metric_count,
                last_indexed_at,
            )| {
                PerProjectStat {
                    project_name,
                    indexed_file_count,
                    chunk_count,
                    file_metric_count,
                    last_indexed_at,
                }
            },
        )
        .collect();

    // Per-project git breakdown: commit_count, last_commit (by date).
    let git_per_project_rows: Vec<GitProjectRow> = sqlx::query_as(
        "SELECT p.name,
                    COUNT(gc.id)::BIGINT,
                    (ARRAY_AGG(gc.commit_hash ORDER BY gc.author_date DESC))[1],
                    MAX(gc.author_date)
             FROM projects p
             LEFT JOIN git_commits gc ON gc.project_id = p.id
             GROUP BY p.id, p.name
             HAVING COUNT(gc.id) > 0
             ORDER BY p.name",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap_or_default();
    let git_per_project: Vec<GitProjectStat> = git_per_project_rows
        .into_iter()
        .map(
            |(project_name, commit_count, last_commit_hash, last_commit_date)| GitProjectStat {
                project_name,
                commit_count,
                last_commit_hash,
                last_commit_date,
            },
        )
        .collect();

    let last_indexed_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT MAX(modified_at) FROM indexed_files")
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(None);

    let server_version: Option<String> = sqlx::query_scalar("SHOW server_version")
        .fetch_one(&mut *tx)
        .await
        .ok();
    let vector_extension_version: Option<String> =
        sqlx::query_scalar("SELECT extversion FROM pg_extension WHERE extname = 'vector'")
            .fetch_one(&mut *tx)
            .await
            .ok();

    tx.commit().await?;

    Ok(StatusSnapshot {
        project_count,
        indexed_file_count,
        chunk_count,
        git_commit_count,
        git_commit_chunk_count,
        topic_count_global,
        topic_count_total,
        topic_assignments_total,
        topic_last_computed,
        topic_noise_chunk_count,
        topic_breakdown_by_scope,
        similarity_pair_count,
        similarity_distinct_files,
        similarity_last_computed,
        file_metric_count,
        graph_edge_count,
        graph_edges_by_type,
        graph_metric_last_computed,
        graph_edge_last_computed,
        blame_coverage_with,
        blame_coverage_total,
        per_project,
        git_per_project,
        last_indexed_at,
        server_version,
        vector_extension_version,
    })
}
