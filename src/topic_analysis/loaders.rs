//! Shared read helpers for the `topic_analysis` collectors. Thin async queries
//! over existing tables (`code_topics`, `chunk_topic_assignments`,
//! `file_chunks`, `indexed_files`) — no writes. Where an existing
//! `crate::db::queries` helper already returns what we need we reuse it; these
//! add only the missing shapes (per-project topic histogram, global theme
//! incidence, scope centroids with a recompute fallback).

use sqlx::PgPool;

use crate::db::queries::TopicCentroidRow;

/// One topic's footprint within a single project: how many of the project's
/// chunks fall under it, plus its label/keywords for display.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ProjectTopicHistogramRow {
    pub topic_id: i32,
    pub label: String,
    pub keywords: Option<Vec<String>>,
    pub chunk_count: i64,
    /// Topic-level cohesion (mean intra-topic similarity), if computed.
    pub avg_internal_similarity: Option<f64>,
}

/// Per-project topic histogram: chunk counts per topic for `project_id`, joined
/// through `chunk_topic_assignments → file_chunks → indexed_files`. The
/// assignment scope is irrelevant — membership is resolved by the file's
/// `project_id`, so this works regardless of the topic's `scope` string.
pub async fn load_project_topic_histogram(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<ProjectTopicHistogramRow>, sqlx::Error> {
    sqlx::query_as::<_, ProjectTopicHistogramRow>(
        "SELECT ct.id AS topic_id, ct.label, ct.keywords,
                ct.avg_internal_similarity, COUNT(*) AS chunk_count
         FROM chunk_topic_assignments cta
         JOIN file_chunks c ON c.id = cta.chunk_id
         JOIN indexed_files f ON f.id = c.file_id
         JOIN code_topics ct ON ct.id = cta.topic_id
         WHERE f.project_id = $1
         GROUP BY ct.id, ct.label, ct.keywords, ct.avg_internal_similarity
         ORDER BY chunk_count DESC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

/// A global roll-up theme and the projects that contribute to it. `project_names`
/// / `project_count` are written by `store_global_rollup`, so this is a single
/// read of `code_topics WHERE scope='global'` — no per-chunk join needed.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct GlobalThemeRow {
    pub topic_id: i32,
    pub label: String,
    pub keywords: Option<Vec<String>>,
    pub project_names: Vec<String>,
    pub project_count: i32,
    pub chunk_count: i32,
}

/// Load the global-scope themes (cross-project roll-ups). Empty when no global
/// roll-up has been computed yet (caller should emit "run discover_topics" guidance).
pub async fn load_global_topic_incidence(
    pool: &PgPool,
) -> Result<Vec<GlobalThemeRow>, sqlx::Error> {
    sqlx::query_as::<_, GlobalThemeRow>(
        "SELECT id AS topic_id, label, keywords,
                COALESCE(project_names, ARRAY[]::text[]) AS project_names,
                COALESCE(project_count, 0) AS project_count,
                chunk_count
         FROM code_topics
         WHERE scope = 'global'
         ORDER BY project_count DESC NULLS LAST, chunk_count DESC",
    )
    .fetch_all(pool)
    .await
}

#[derive(Debug, sqlx::FromRow)]
struct StoredCentroidRow {
    topic_id: i32,
    label: String,
    chunk_count: i32,
    centroid: Vec<f32>,
}

/// Load per-topic centroids for a `scope` (e.g. `"project:NAME"`), preferring the
/// **stored** `code_topics.centroid` (one query, cheap) and falling back to
/// `load_topic_centroids` (recompute-from-embeddings) when centroids are NULL —
/// older data or the synthetic test fixture, which leaves the column NULL.
/// Centroids live in the shared BGE-M3 embedding space, so they are comparable
/// across projects.
pub async fn load_scope_centroids(
    pool: &PgPool,
    scope: &str,
) -> Result<Vec<TopicCentroidRow>, sqlx::Error> {
    let stored = sqlx::query_as::<_, StoredCentroidRow>(
        "SELECT id AS topic_id, label, chunk_count, centroid
         FROM code_topics
         WHERE scope = $1 AND centroid IS NOT NULL AND array_length(centroid, 1) > 0",
    )
    .bind(scope)
    .fetch_all(pool)
    .await?;

    if stored.is_empty() {
        // No stored centroids (fixture / pre-centroid data) → recompute.
        return crate::db::queries::load_topic_centroids(pool, scope).await;
    }

    Ok(stored
        .into_iter()
        .map(|r| TopicCentroidRow {
            topic_id: r.topic_id,
            label: r.label,
            chunk_count: r.chunk_count as i64,
            centroid: r.centroid,
        })
        .collect())
}

/// Load a project's chunk embeddings from the active embedding column, capped
/// (deterministic by chunk id) to bound cost on large projects. Scope-agnostic:
/// joins `file_chunks → indexed_files` by `project_id`, so it works regardless
/// of the topic scope. Used to build a project's content vector (mean) and its
/// distribution over the global themes.
pub async fn load_project_chunk_embeddings(
    pool: &PgPool,
    project_id: i32,
    cap: i64,
) -> Result<Vec<Vec<f32>>, sqlx::Error> {
    let col = crate::embed::signature::read_active_signature(pool)
        .await?
        .read_column();
    sqlx::query_scalar::<_, Vec<f32>>(sqlx::AssertSqlSafe(format!(
        "SELECT c.{col}::real[] AS embedding
         FROM file_chunks c
         JOIN indexed_files f ON f.id = c.file_id
         WHERE f.project_id = $1 AND c.{col} IS NOT NULL
         ORDER BY c.id
         LIMIT $2",
    )))
    .bind(project_id)
    .bind(cap)
    .fetch_all(pool)
    .await
}

/// An undirected topic co-occurrence edge: how many of a project's chunks are
/// assigned to BOTH topics (a chunk may carry up to `MAX_MEMBERSHIPS_PER_CHUNK`
/// soft memberships).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TopicCoEdge {
    pub topic_a: i32,
    pub topic_b: i32,
    pub co_count: i64,
}

/// Topic co-occurrence edges within a project: chunks co-assigned to two topics.
/// Ordered pairs (`a < b`) deduplicated; filtered to `>= min_weight` shared chunks.
pub async fn load_topic_cooccurrence_edges(
    pool: &PgPool,
    project_id: i32,
    min_weight: i64,
) -> Result<Vec<TopicCoEdge>, sqlx::Error> {
    sqlx::query_as::<_, TopicCoEdge>(
        "SELECT a.topic_id AS topic_a, b.topic_id AS topic_b,
                COUNT(DISTINCT a.chunk_id) AS co_count
         FROM chunk_topic_assignments a
         JOIN chunk_topic_assignments b
              ON a.chunk_id = b.chunk_id AND a.topic_id < b.topic_id
         JOIN file_chunks c ON c.id = a.chunk_id
         JOIN indexed_files f ON f.id = c.file_id
         WHERE f.project_id = $1
         GROUP BY a.topic_id, b.topic_id
         HAVING COUNT(DISTINCT a.chunk_id) >= $2
         ORDER BY co_count DESC",
    )
    .bind(project_id)
    .bind(min_weight)
    .fetch_all(pool)
    .await
}

/// Per-(topic, author) chunk counts within a project, from git blame. Used to
/// derive per-topic ownership / bus-factor. Mirrors the blame join in
/// `find_bus_factor_files` (`db::queries::metrics`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TopicAuthorRow {
    pub topic_id: i32,
    pub label: String,
    pub author: String,
    pub chunk_count: i64,
}

/// Load per-topic author contribution (chunk counts) for a project. Only chunks
/// with a `blame_author` are counted (returns empty when blame is unpopulated).
pub async fn load_topic_author_lines(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<TopicAuthorRow>, sqlx::Error> {
    sqlx::query_as::<_, TopicAuthorRow>(
        "SELECT cta.topic_id, ct.label, fc.blame_author AS author,
                COUNT(*) AS chunk_count
         FROM chunk_topic_assignments cta
         JOIN file_chunks fc ON fc.id = cta.chunk_id
         JOIN indexed_files f ON f.id = fc.file_id
         JOIN code_topics ct ON ct.id = cta.topic_id
         WHERE f.project_id = $1 AND fc.blame_author IS NOT NULL
         GROUP BY cta.topic_id, ct.label, fc.blame_author
         ORDER BY cta.topic_id, chunk_count DESC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

/// Per-topic chunk counts split into "recent" (`blame_date` within `recent_days`)
/// vs "prior", across all topic assignments. An age-distribution proxy for theme
/// growth/decline used by `topic_trends` (mode=chunk_age). Empty without blame.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TopicAgeSplit {
    pub topic_id: i32,
    pub label: String,
    pub recent: i64,
    pub prior: i64,
}

pub async fn load_topic_age_split(
    pool: &PgPool,
    recent_days: i32,
) -> Result<Vec<TopicAgeSplit>, sqlx::Error> {
    sqlx::query_as::<_, TopicAgeSplit>(
        "SELECT cta.topic_id, ct.label,
                COUNT(*) FILTER (WHERE fc.blame_date >= NOW() - make_interval(days => $1)) AS recent,
                COUNT(*) FILTER (WHERE fc.blame_date <  NOW() - make_interval(days => $1)) AS prior
         FROM chunk_topic_assignments cta
         JOIN file_chunks fc ON fc.id = cta.chunk_id
         JOIN code_topics ct ON ct.id = cta.topic_id
         WHERE fc.blame_date IS NOT NULL
         GROUP BY cta.topic_id, ct.label",
    )
    .bind(recent_days)
    .fetch_all(pool)
    .await
}
