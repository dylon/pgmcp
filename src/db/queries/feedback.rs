//! Queries for `agent_feedback` (ADR-023, schema v43).
//!
//! Insert (embed-on-write via [`set_feedback_embedding`]), filter
//! ([`list_feedback`]), retrieve ([`get_feedback`]), full-text + semantic search
//! ([`search_feedback_fts`] / [`search_feedback_semantic`]), triage
//! ([`respond_feedback`]), and promotion-linking ([`mark_feedback_promoted`]).

use pgvector::Vector;
use sqlx::PgPool;

/// Columns returned for a feedback row (the `embedding` vector is intentionally
/// not selected — callers never need the raw vector back).
const FEEDBACK_COLS: &str = "id, agent_id, category, sentiment, subject, body, about_tool, \
     project_id, status, responded_by, response, promoted_work_item_id, created_at, updated_at";

/// A feedback row as returned to tools.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct FeedbackRow {
    pub id: i64,
    pub agent_id: String,
    pub category: String,
    pub sentiment: String,
    pub subject: Option<String>,
    pub body: String,
    pub about_tool: Option<String>,
    pub project_id: Option<i32>,
    pub status: String,
    pub responded_by: Option<String>,
    pub response: Option<String>,
    pub promoted_work_item_id: Option<i64>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// A new feedback submission (vocabularies validated by the caller).
pub struct NewFeedback<'a> {
    pub agent_id: &'a str,
    pub category: &'a str,
    pub sentiment: &'a str,
    pub subject: Option<&'a str>,
    pub body: &'a str,
    pub about_tool: Option<&'a str>,
    pub project_id: Option<i32>,
}

/// Insert a feedback row (embedding left NULL — set by [`set_feedback_embedding`]
/// in the same tool call, or backfilled later). Returns the new id.
pub async fn insert_feedback(pool: &PgPool, f: NewFeedback<'_>) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO agent_feedback
            (agent_id, category, sentiment, subject, body, about_tool, project_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING id",
    )
    .bind(f.agent_id)
    .bind(f.category)
    .bind(f.sentiment)
    .bind(f.subject)
    .bind(f.body)
    .bind(f.about_tool)
    .bind(f.project_id)
    .fetch_one(pool)
    .await
}

/// Store the embed-on-write vector for a feedback row. The `embedding_signature`
/// keeps its column default (the active model), mirroring the work-item
/// embed-on-write convention; the embedding-migration cron re-embeds if the
/// active signature ever advances.
pub async fn set_feedback_embedding(
    pool: &PgPool,
    id: i64,
    embedding: Vector,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE agent_feedback SET embedding = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(embedding)
        .execute(pool)
        .await?;
    Ok(())
}

/// Fetch one feedback row by id.
pub async fn get_feedback(pool: &PgPool, id: i64) -> Result<Option<FeedbackRow>, sqlx::Error> {
    sqlx::query_as::<_, FeedbackRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {FEEDBACK_COLS} FROM agent_feedback WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// List feedback with optional filters (each NULL filter is a wildcard).
#[allow(clippy::too_many_arguments)]
pub async fn list_feedback(
    pool: &PgPool,
    category: Option<&str>,
    sentiment: Option<&str>,
    status: Option<&str>,
    about_tool: Option<&str>,
    project_id: Option<i32>,
    limit: i64,
) -> Result<Vec<FeedbackRow>, sqlx::Error> {
    sqlx::query_as::<_, FeedbackRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {FEEDBACK_COLS} FROM agent_feedback
          WHERE ($1::text IS NULL OR category = $1)
            AND ($2::text IS NULL OR sentiment = $2)
            AND ($3::text IS NULL OR status = $3)
            AND ($4::text IS NULL OR about_tool = $4)
            AND ($5::int IS NULL OR project_id = $5)
          ORDER BY created_at DESC
          LIMIT $6"
    )))
    .bind(category)
    .bind(sentiment)
    .bind(status)
    .bind(about_tool)
    .bind(project_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Full-text search over subject + body.
pub async fn search_feedback_fts(
    pool: &PgPool,
    query: &str,
    limit: i64,
) -> Result<Vec<FeedbackRow>, sqlx::Error> {
    sqlx::query_as::<_, FeedbackRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {FEEDBACK_COLS} FROM agent_feedback
          WHERE to_tsvector('english', coalesce(subject, '') || ' ' || body)
                @@ plainto_tsquery('english', $1)
          ORDER BY created_at DESC
          LIMIT $2"
    )))
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Semantic (vector) search over embedded feedback rows.
pub async fn search_feedback_semantic(
    pool: &PgPool,
    embedding: Vector,
    limit: i64,
) -> Result<Vec<FeedbackRow>, sqlx::Error> {
    sqlx::query_as::<_, FeedbackRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {FEEDBACK_COLS} FROM agent_feedback
          WHERE embedding IS NOT NULL
          ORDER BY embedding <=> $1
          LIMIT $2"
    )))
    .bind(embedding)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Triage a feedback item: set status and (optionally) a response. Returns true
/// if a row was updated.
pub async fn respond_feedback(
    pool: &PgPool,
    id: i64,
    status: &str,
    responded_by: &str,
    response: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE agent_feedback
            SET status = $2, responded_by = $3,
                response = COALESCE($4, response), updated_at = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(responded_by)
    .bind(response)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Record that a feedback item was promoted into a work-item (and mark it
/// `planned`). Returns true if a row was updated.
pub async fn mark_feedback_promoted(
    pool: &PgPool,
    id: i64,
    work_item_id: i64,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE agent_feedback
            SET promoted_work_item_id = $2, status = 'planned', updated_at = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(work_item_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}
