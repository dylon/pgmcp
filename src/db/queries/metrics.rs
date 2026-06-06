//! File-level risk/complexity/ownership metrics (call-site counts, zombie
//! candidates, god-file chunks, hot paths, bus-factor, merge-conflict risk,
//! growth buckets, AST/function aggregates). Extracted from `queries.rs` (god-file split).
#![allow(unused_imports)]

use crate::db::queries::*;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// One row per file with the count of distinct source files importing it.
/// Used by `extraction_candidates` to estimate `effort.call_sites_to_update`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CallSiteCount {
    pub file_id: i64,
    pub importer_count: i64,
    /// Approximate count of unresolved imports (target_raw match by basename).
    /// Filled when the symbol_references table is empty AND the language has
    /// no resolved-import support (Go/Java/C/C++ pre-Tier-0e).
    pub unresolved_count: i64,
}

/// For each input file_id, count the distinct files that import it.
/// Resolved-only path: joins `code_graph_edges` where `target_file_id`
/// already resolved at graph-analysis time. Unresolved-target imports
/// (Go/Java/C/C++) are reported as `unresolved_count` via fuzzy basename
/// match — it's an upper bound, hence the dedicated field.
pub async fn count_call_sites_to_files(
    pool: &PgPool,
    file_ids: &[i64],
) -> Result<Vec<CallSiteCount>, sqlx::Error> {
    if file_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, CallSiteCount>(
        "WITH targets AS (
            SELECT id, regexp_replace(relative_path, '^.*/', '') AS basename
            FROM indexed_files
            WHERE id = ANY($1)
         ),
         resolved AS (
            SELECT t.id AS file_id,
                   COUNT(DISTINCT cge.source_file_id) AS importer_count
            FROM targets t
            LEFT JOIN code_graph_edges cge
                  ON cge.target_file_id = t.id AND cge.edge_type = 'import'
            GROUP BY t.id
         ),
         unresolved AS (
            SELECT t.id AS file_id,
                   COUNT(DISTINCT cge.source_file_id) AS unresolved_count
            FROM targets t
            LEFT JOIN code_graph_edges cge
                  ON cge.target_file_id IS NULL
                 AND cge.edge_type = 'import'
                 AND cge.target_raw ILIKE '%' || regexp_replace(t.basename, '\\.[^.]+$', '') || '%'
            GROUP BY t.id
         )
         SELECT r.file_id,
                COALESCE(r.importer_count, 0) AS importer_count,
                COALESCE(u.unresolved_count, 0) AS unresolved_count
         FROM resolved r
         LEFT JOIN unresolved u ON u.file_id = r.file_id",
    )
    .bind(file_ids)
    .fetch_all(pool)
    .await
}

/// Subset of `file_metrics` columns used for risk-tier classification in
/// `extraction_candidates`. Returns one row per requested file_id; files
/// without a `file_metrics` row are simply absent.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FileRiskMetrics {
    pub file_id: i64,
    pub pagerank: Option<f64>,
    pub churn_rate: Option<f64>,
    pub fix_commit_ratio: Option<f64>,
    pub days_since_last_change: Option<i32>,
}

/// Pull churn / pagerank / fix-ratio for a set of file_ids in one query.
pub async fn get_file_risk_metrics(
    pool: &PgPool,
    file_ids: &[i64],
) -> Result<Vec<FileRiskMetrics>, sqlx::Error> {
    if file_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, FileRiskMetrics>(
        "SELECT file_id, pagerank, churn_rate, fix_commit_ratio, days_since_last_change
         FROM file_metrics
         WHERE file_id = ANY($1)",
    )
    .bind(file_ids)
    .fetch_all(pool)
    .await
}

/// One row per zombie-candidate file: low PageRank percentile, low in-degree,
/// long-idle. Used by `stale_zombie_detector`.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ZombieCandidate {
    pub file_id: i64,
    pub relative_path: String,
    pub line_count: i32,
    pub pagerank: Option<f64>,
    pub pagerank_pct: f64,
    pub in_degree: Option<i32>,
    pub author_count: Option<i32>,
    pub commit_count: Option<i32>,
    pub days_since_last_change: Option<i32>,
}

/// Find files that are graph + history "zombies": low PageRank, low in-degree,
/// long-idle. Distinct from `find_orphans` (topic-based) — this combines
/// graph centrality, import topology, and authorial abandonment.
pub async fn find_zombie_candidates(
    pool: &PgPool,
    project_name: &str,
    min_days_idle: i32,
    max_pagerank_pct: f64,
    limit: i32,
) -> Result<Vec<ZombieCandidate>, sqlx::Error> {
    sqlx::query_as::<_, ZombieCandidate>(
        "WITH ranked AS (
            SELECT f.id AS file_id,
                   f.relative_path,
                   f.line_count,
                   fm.pagerank,
                   fm.in_degree,
                   fm.author_count,
                   fm.commit_count,
                   fm.days_since_last_change,
                   PERCENT_RANK() OVER (ORDER BY COALESCE(fm.pagerank, 0)) AS pagerank_pct
            FROM indexed_files f
            JOIN projects p ON p.id = f.project_id
            LEFT JOIN file_metrics fm ON fm.file_id = f.id
            WHERE p.name = $1
         )
         SELECT file_id, relative_path, line_count, pagerank, pagerank_pct,
                in_degree, author_count, commit_count, days_since_last_change
         FROM ranked
         WHERE COALESCE(in_degree, 0) <= 1
           AND COALESCE(days_since_last_change, 0) > $2
           AND pagerank_pct <= $3
         ORDER BY pagerank_pct ASC,
                  COALESCE(days_since_last_change, 0) DESC,
                  file_id ASC
         LIMIT $4",
    )
    .bind(project_name)
    .bind(min_days_idle)
    .bind(max_pagerank_pct)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// One row per chunk in a god-candidate file, with its dominant FCM topic
/// (highest membership_score) when one is known. Used by
/// `recommend_module_split` to group chunks into proposed sub-files.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct GodFileChunkRow {
    pub file_id: i64,
    pub relative_path: String,
    pub language: String,
    pub line_count: i32,
    pub chunk_id: i64,
    pub chunk_index: i32,
    pub start_line: i32,
    pub end_line: i32,
    pub topic_id: Option<i64>,
    pub topic_label: Option<String>,
    pub topic_keywords: Option<Vec<String>>,
    pub membership_score: Option<f64>,
}

/// For a project, return all chunks of files whose `line_count >= min_lines`,
/// each annotated with the chunk's dominant FCM topic (the assignment row
/// with the highest `membership_score`). Topics may be NULL when no FCM run
/// has reached that chunk yet.
///
/// Drives `recommend_module_split` — chunks of a god file get grouped by
/// `topic_id` to produce per-topic sub-file recommendations.
pub async fn get_god_file_chunks_with_topics(
    pool: &PgPool,
    project_name: &str,
    min_lines: i32,
) -> Result<Vec<GodFileChunkRow>, sqlx::Error> {
    sqlx::query_as::<_, GodFileChunkRow>(
        "WITH god_files AS (
            SELECT f.id, f.relative_path, f.language, f.line_count
            FROM indexed_files f
            JOIN projects p ON p.id = f.project_id
            WHERE p.name = $1 AND f.line_count >= $2
         ),
         dominant_topic AS (
            SELECT DISTINCT ON (cta.chunk_id)
                   cta.chunk_id,
                   cta.topic_id,
                   cta.membership_score,
                   ct.label,
                   ct.keywords
            FROM chunk_topic_assignments cta
            JOIN code_topics ct ON ct.id = cta.topic_id
            ORDER BY cta.chunk_id, cta.membership_score DESC, cta.topic_id ASC
         )
         SELECT g.id AS file_id,
                g.relative_path,
                g.language,
                g.line_count,
                fc.id AS chunk_id,
                fc.chunk_index,
                fc.start_line,
                fc.end_line,
                dt.topic_id,
                dt.label AS topic_label,
                dt.keywords AS topic_keywords,
                dt.membership_score
         FROM god_files g
         JOIN file_chunks fc ON fc.file_id = g.id
         LEFT JOIN dominant_topic dt ON dt.chunk_id = fc.id
         ORDER BY g.id, fc.chunk_index",
    )
    .bind(project_name)
    .bind(min_lines)
    .fetch_all(pool)
    .await
}

// ============================================================================
// Tier 4 — engineer/architect workflow queries
// ============================================================================

/// One row per "hot path" file: high PageRank, high churn, high fix-commit ratio.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct HotPathRow {
    pub file_id: i64,
    pub relative_path: String,
    pub pagerank: Option<f64>,
    pub churn_rate: Option<f64>,
    pub fix_commit_ratio: Option<f64>,
    pub bug_proneness: Option<f64>,
    pub instability: Option<f64>,
    pub in_degree: Option<i32>,
    pub author_count: Option<i32>,
    pub commit_count: Option<i32>,
    pub pagerank_pct: f64,
    pub churn_pct: f64,
    pub fix_ratio_pct: f64,
}

/// Files in the intersection of top-P% PageRank, top-P% churn, and top-P%
/// fix_commit_ratio for a project. Used by `hot_path_audit`.
pub async fn find_hot_paths(
    pool: &PgPool,
    project_name: &str,
    percentile_threshold: f64,
    limit: i32,
) -> Result<Vec<HotPathRow>, sqlx::Error> {
    sqlx::query_as::<_, HotPathRow>(
        "WITH stats AS (
            SELECT f.id AS file_id,
                   f.relative_path,
                   fm.pagerank,
                   fm.churn_rate,
                   fm.fix_commit_ratio,
                   fm.bug_proneness,
                   fm.instability,
                   fm.in_degree,
                   fm.author_count,
                   fm.commit_count,
                   PERCENT_RANK() OVER (ORDER BY COALESCE(fm.pagerank, 0)) AS pagerank_pct,
                   PERCENT_RANK() OVER (ORDER BY COALESCE(fm.churn_rate, 0)) AS churn_pct,
                   PERCENT_RANK() OVER (ORDER BY COALESCE(fm.fix_commit_ratio, 0)) AS fix_ratio_pct
            FROM indexed_files f
            JOIN projects p ON p.id = f.project_id
            LEFT JOIN file_metrics fm ON fm.file_id = f.id
            WHERE p.name = $1
         )
         SELECT file_id, relative_path,
                pagerank, churn_rate, fix_commit_ratio,
                bug_proneness, instability,
                in_degree, author_count, commit_count,
                pagerank_pct, churn_pct, fix_ratio_pct
         FROM stats
         WHERE pagerank_pct >= $2
           AND churn_pct >= $2
           AND fix_ratio_pct >= $2
         ORDER BY (pagerank_pct + churn_pct + fix_ratio_pct) DESC,
                  file_id ASC
         LIMIT $3",
    )
    .bind(project_name)
    .bind(percentile_threshold)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Files in the intersection of top-P% PageRank, top-P% churn, and top-P%
/// fix_commit_ratio for one already-resolved project id.
pub async fn find_hot_paths_by_project_id(
    pool: &PgPool,
    project_id: i32,
    percentile_threshold: f64,
    limit: i32,
) -> Result<Vec<HotPathRow>, sqlx::Error> {
    sqlx::query_as::<_, HotPathRow>(
        "WITH stats AS (
            SELECT f.id AS file_id,
                   f.relative_path,
                   fm.pagerank,
                   fm.churn_rate,
                   fm.fix_commit_ratio,
                   fm.bug_proneness,
                   fm.instability,
                   fm.in_degree,
                   fm.author_count,
                   fm.commit_count,
                   PERCENT_RANK() OVER (ORDER BY COALESCE(fm.pagerank, 0)) AS pagerank_pct,
                   PERCENT_RANK() OVER (ORDER BY COALESCE(fm.churn_rate, 0)) AS churn_pct,
                   PERCENT_RANK() OVER (ORDER BY COALESCE(fm.fix_commit_ratio, 0)) AS fix_ratio_pct
            FROM indexed_files f
            LEFT JOIN file_metrics fm ON fm.file_id = f.id AND fm.project_id = f.project_id
            WHERE f.project_id = $1
         )
         SELECT file_id, relative_path,
                pagerank, churn_rate, fix_commit_ratio,
                bug_proneness, instability,
                in_degree, author_count, commit_count,
                pagerank_pct, churn_pct, fix_ratio_pct
         FROM stats
         WHERE pagerank_pct >= $2
           AND churn_pct >= $2
           AND fix_ratio_pct >= $2
         ORDER BY (pagerank_pct + churn_pct + fix_ratio_pct) DESC,
                  file_id ASC
         LIMIT $3",
    )
    .bind(project_id)
    .bind(percentile_threshold)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// One row per file with its top author (by lines blamed) and risk score.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BusFactorRow {
    pub file_id: i64,
    pub relative_path: String,
    pub top_author: String,
    pub top_share: f64,
    pub distinct_authors: i64,
    pub last_touch: Option<DateTime<Utc>>,
    pub pagerank: Option<f64>,
    pub risk_score: Option<f64>,
}

/// Per-file bus-factor risk for a project: top author's share of blamed lines
/// × pagerank ÷ author count. Used by `bus_factor_map`.
pub async fn find_bus_factor_files(
    pool: &PgPool,
    project_name: &str,
    min_pagerank_pct: f64,
    limit: i32,
) -> Result<Vec<BusFactorRow>, sqlx::Error> {
    sqlx::query_as::<_, BusFactorRow>(
        "WITH per_file AS (
            SELECT f.id,
                   f.relative_path,
                   fc.blame_author,
                   COUNT(*) AS lines_blamed,
                   MAX(fc.blame_date) AS last_touch
            FROM file_chunks fc
            JOIN indexed_files f ON f.id = fc.file_id
            JOIN projects p ON p.id = f.project_id
            WHERE p.name = $1 AND fc.blame_author IS NOT NULL
            GROUP BY f.id, f.relative_path, fc.blame_author
         ),
         top AS (
            SELECT id,
                   relative_path,
                   (array_agg(blame_author ORDER BY lines_blamed DESC))[1] AS top_author,
                   (MAX(lines_blamed)::float8) /
                       NULLIF(SUM(lines_blamed)::float8, 0) AS top_share,
                   COUNT(*)::bigint AS distinct_authors,
                   MAX(last_touch) AS last_touch
            FROM per_file
            GROUP BY id, relative_path
         ),
         ranked AS (
            SELECT t.*,
                   fm.pagerank,
                   PERCENT_RANK() OVER (ORDER BY COALESCE(fm.pagerank, 0)) AS pr_pct
            FROM top t
            LEFT JOIN file_metrics fm ON fm.file_id = t.id
         )
         SELECT id AS file_id,
                relative_path,
                top_author,
                top_share,
                distinct_authors,
                last_touch,
                pagerank,
                (COALESCE(pagerank, 0.0) * top_share /
                    GREATEST(1.0, distinct_authors::float8)) AS risk_score
         FROM ranked
         WHERE pr_pct >= $2
         ORDER BY risk_score DESC NULLS LAST, file_id ASC
         LIMIT $3",
    )
    .bind(project_name)
    .bind(min_pagerank_pct)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// One row per (file, top_author) pair within the recency window. Used by
/// `reviewer_recommender` to aggregate per-author file ownership.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FileAuthorRow {
    pub relative_path: String,
    pub top_author: Option<String>,
    pub last_touch_days: Option<i32>,
}

/// For each requested file, return the dominant blame_author within the
/// recency window. Files without blame coverage return `top_author = NULL`.
pub async fn find_dominant_authors_for_files(
    pool: &PgPool,
    project_name: &str,
    file_paths: &[String],
    recency_window_days: i32,
) -> Result<Vec<FileAuthorRow>, sqlx::Error> {
    if file_paths.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, FileAuthorRow>(
        "WITH per_file AS (
            SELECT f.relative_path,
                   fc.blame_author,
                   COUNT(*) AS lines_blamed,
                   MAX(fc.blame_date) AS last_touch
            FROM file_chunks fc
            JOIN indexed_files f ON f.id = fc.file_id
            JOIN projects p ON p.id = f.project_id
            WHERE p.name = $1
              AND f.relative_path = ANY($2)
              AND fc.blame_author IS NOT NULL
              AND fc.blame_date >= NOW() - ($3 || ' days')::interval
            GROUP BY f.relative_path, fc.blame_author
         )
         SELECT relative_path,
                (array_agg(blame_author ORDER BY lines_blamed DESC))[1] AS top_author,
                EXTRACT(DAY FROM (NOW() - MAX(last_touch)))::int AS last_touch_days
         FROM per_file
         GROUP BY relative_path",
    )
    .bind(project_name)
    .bind(file_paths)
    .bind(recency_window_days)
    .fetch_all(pool)
    .await
}

// ============================================================================
// Tier 5 — audit & trend queries
// ============================================================================

/// One row per unresolved external dep target. Used by `dependency_health`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UnresolvedDepRow {
    pub target_raw: String,
    pub importer_count: i64,
    pub usage_centrality: f64,
    pub latest_change_days: Option<f64>,
    pub sample_importers: Vec<String>,
}

/// External dependency-target audit. Groups `code_graph_edges` rows where
/// `target_file_id IS NULL` (unresolved imports — typically external crates,
/// system libraries, or Go/Java/C/C++ targets pre-Tier-0e) by `target_raw`.
pub async fn find_unresolved_dependencies(
    pool: &PgPool,
    project_id: Option<i32>,
    limit: i32,
) -> Result<Vec<UnresolvedDepRow>, sqlx::Error> {
    sqlx::query_as::<_, UnresolvedDepRow>(
        "SELECT cge.target_raw,
                COUNT(DISTINCT cge.source_file_id) AS importer_count,
                COALESCE(SUM(COALESCE(fm.pagerank, 0.0)), 0.0) AS usage_centrality,
                EXTRACT(EPOCH FROM (NOW() - MAX(f.indexed_at)))/86400.0 AS latest_change_days,
                (array_agg(DISTINCT f.relative_path))[1:5] AS sample_importers
         FROM code_graph_edges cge
         JOIN indexed_files f ON f.id = cge.source_file_id
         LEFT JOIN file_metrics fm ON fm.file_id = cge.source_file_id
         WHERE cge.target_file_id IS NULL
           AND cge.target_raw IS NOT NULL
           AND cge.edge_type = 'import'
           AND ($1::int IS NULL OR cge.project_id = $1)
         GROUP BY cge.target_raw
         ORDER BY usage_centrality DESC NULLS LAST, importer_count DESC, target_raw ASC
         LIMIT $2",
    )
    .bind(project_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// One row per file in the merge-conflict scan. Used by `merge_conflict_risk`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MergeRiskRow {
    pub file_path: String,
    pub recent_commits: i64,
    pub distinct_recent_authors: i64,
    pub top_other_authors: Vec<String>,
}

/// Find files in `branch_files` with overlapping recent commits from other
/// authors. Used by `merge_conflict_risk`. The `exclude_email` is omitted
/// from the per-file partner counts.
pub async fn find_merge_conflict_risks(
    pool: &PgPool,
    project_name: &str,
    branch_files: &[String],
    window_days: i32,
    exclude_email: Option<&str>,
) -> Result<Vec<MergeRiskRow>, sqlx::Error> {
    if branch_files.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, MergeRiskRow>(
        "WITH commits_in_window AS (
            SELECT gc.author, gcf.file_path
            FROM git_commits gc
            JOIN git_commit_files gcf ON gcf.commit_id = gc.id
            JOIN projects p ON p.id = gc.project_id
            WHERE p.name = $1
              AND gc.author_date >= NOW() - ($3 || ' days')::interval
              AND gcf.file_path = ANY($2)
              AND ($4::text IS NULL OR gc.author <> $4)
         )
         SELECT file_path,
                COUNT(*)::bigint AS recent_commits,
                COUNT(DISTINCT author)::bigint AS distinct_recent_authors,
                (array_agg(DISTINCT author))[1:5] AS top_other_authors
         FROM commits_in_window
         GROUP BY file_path
         ORDER BY distinct_recent_authors DESC, recent_commits DESC, file_path ASC",
    )
    .bind(project_name)
    .bind(branch_files)
    .bind(window_days)
    .bind(exclude_email)
    .fetch_all(pool)
    .await
}

/// One time-bucket row for a project (or single file). Used by `module_growth_trajectory`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct GrowthBucketRow {
    pub period_start: DateTime<Utc>,
    pub commits: i64,
    pub authors: i64,
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
}

/// Bucket commits into time periods (week/month/quarter) and aggregate.
/// `interval_unit` is one of "week", "month", "quarter" — caller validates.
pub async fn get_growth_buckets(
    pool: &PgPool,
    project_name: &str,
    file_path: Option<&str>,
    interval_unit: &str,
    lookback_buckets: i32,
) -> Result<Vec<GrowthBucketRow>, sqlx::Error> {
    sqlx::query_as::<_, GrowthBucketRow>(
        // We can't bind interval keywords directly; concat-and-cast is the
        // typical workaround. interval_unit is hard-coded to one of three
        // strings by the caller, so injection isn't a concern.
        &format!(
            "WITH per_commit AS (
                SELECT gc.author_date,
                       gc.author,
                       gc.id,
                       date_trunc('{unit}', gc.author_date) AS bucket
                FROM git_commits gc
                JOIN projects p ON p.id = gc.project_id
                LEFT JOIN git_commit_files gcf ON gcf.commit_id = gc.id
                WHERE p.name = $1
                  AND gc.author_date >= NOW() - ($3 * INTERVAL '1 {unit}')
                  AND ($2::text IS NULL OR gcf.file_path = $2)
                GROUP BY gc.id, gc.author_date, gc.author
             )
             SELECT bucket AS period_start,
                    COUNT(*)::bigint AS commits,
                    COUNT(DISTINCT author)::bigint AS authors,
                    NULL::bigint AS additions,
                    NULL::bigint AS deletions
             FROM per_commit
             GROUP BY bucket
             ORDER BY bucket ASC",
            unit = interval_unit
        ),
    )
    .bind(project_name)
    .bind(file_path)
    .bind(lookback_buckets)
    .fetch_all(pool)
    .await
}

/// File complexity data for hotspot analysis.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct FileComplexityRow {
    pub path: String,
    pub language: String,
    pub size_bytes: i64,
    pub chunk_count: i64,
    pub topic_count: i64,
}

/// Get per-file complexity data (size, chunk count, topic diversity).
pub async fn get_file_complexity_data(
    pool: &PgPool,
    project: &str,
) -> Result<Vec<FileComplexityRow>, sqlx::Error> {
    sqlx::query_as::<_, FileComplexityRow>(
        "SELECT f.path, f.language, f.size_bytes,
                COUNT(DISTINCT c.id) as chunk_count,
                COUNT(DISTINCT cta.topic_id) as topic_count
         FROM indexed_files f
         JOIN projects p ON p.id = f.project_id
         JOIN file_chunks c ON c.file_id = f.id
         LEFT JOIN chunk_topic_assignments cta ON cta.chunk_id = c.id
         WHERE p.name = $1
         GROUP BY f.id, f.path, f.language, f.size_bytes
         ORDER BY chunk_count DESC",
    )
    .bind(project)
    .fetch_all(pool)
    .await
}

/// Get per-file complexity data for a resolved project id.
pub async fn get_file_complexity_data_by_project_id(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<FileComplexityRow>, sqlx::Error> {
    sqlx::query_as::<_, FileComplexityRow>(
        "SELECT f.path, f.language, f.size_bytes,
                COUNT(DISTINCT c.id) as chunk_count,
                COUNT(DISTINCT cta.topic_id) as topic_count
         FROM indexed_files f
         JOIN file_chunks c ON c.file_id = f.id
         LEFT JOIN chunk_topic_assignments cta ON cta.chunk_id = c.id
         WHERE f.project_id = $1
         GROUP BY f.id, f.path, f.language, f.size_bytes
         ORDER BY chunk_count DESC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

/// Per-file aggregate of the rigorous per-function `function_metrics` (real AST
/// cyclomatic / cognitive / Halstead / Maintainability-Index). Lets
/// `design_metrics` and `complexity_hotspots` emit AST-grade values for parsed
/// files instead of their regex/line-count heuristics. `sum_cyclomatic` is the
/// true Chidamber-Kemerer WMC (Σ method complexity); `max_cyclomatic` is the
/// file's worst single function.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FileFunctionAggregate {
    pub file_id: i64,
    pub function_count: i64,
    pub sum_cyclomatic: i64,
    pub max_cyclomatic: i32,
    pub sum_cognitive: i64,
    pub sum_halstead_volume: f64,
    pub avg_maintainability: f64,
    pub min_maintainability: f64,
}

/// Aggregate `function_metrics` per file for a project. Files with no parsed
/// functions simply don't appear (callers fall back to their heuristic).
pub async fn get_file_function_metric_aggregates(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<FileFunctionAggregate>, sqlx::Error> {
    sqlx::query_as::<_, FileFunctionAggregate>(
        "SELECT fm.file_id,
                COUNT(*)                                      AS function_count,
                COALESCE(SUM(fm.cyclomatic), 0)               AS sum_cyclomatic,
                COALESCE(MAX(fm.cyclomatic), 0)               AS max_cyclomatic,
                COALESCE(SUM(fm.cognitive), 0)                AS sum_cognitive,
                COALESCE(SUM(fm.halstead_volume), 0.0)        AS sum_halstead_volume,
                COALESCE(AVG(fm.maintainability_index), 100.0) AS avg_maintainability,
                COALESCE(MIN(fm.maintainability_index), 100.0) AS min_maintainability
         FROM function_metrics fm
         WHERE fm.project_id = $1
         GROUP BY fm.file_id",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

/// Per-file AST-complexity summary keyed by `relative_path` (for tools that
/// work in path space, e.g. `complexity_hotspots`): `(relative_path,
/// max_cyclomatic, min_maintainability, function_count)`. Files with no parsed
/// functions are absent.
pub async fn get_file_ast_complexity_by_path(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<(String, i32, f64, i64)>, sqlx::Error> {
    sqlx::query_as::<_, (String, i32, f64, i64)>(
        "SELECT f.relative_path,
                COALESCE(MAX(fm.cyclomatic), 0)                AS max_cyclomatic,
                COALESCE(MIN(fm.maintainability_index), 100.0) AS min_maintainability,
                COUNT(*)                                       AS function_count
         FROM function_metrics fm
         JOIN indexed_files f ON f.id = fm.file_id
         WHERE fm.project_id = $1
         GROUP BY f.relative_path",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}
