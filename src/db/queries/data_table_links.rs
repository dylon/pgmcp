//! Queries for `data_table_links` (ADR-023, schema v44) — the bridge tying a
//! data table to the experiment / work-item it backs.

use sqlx::PgPool;

/// Link a data table to a target (idempotent upsert; re-linking updates `role`).
/// Returns the row id.
pub async fn link_data_table(
    pool: &PgPool,
    table_id: i64,
    target_type: &str,
    target_id: i64,
    role: Option<&str>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO data_table_links (table_id, target_type, target_id, role)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (table_id, target_type, target_id)
         DO UPDATE SET role = EXCLUDED.role
         RETURNING id",
    )
    .bind(table_id)
    .bind(target_type)
    .bind(target_id)
    .bind(role)
    .fetch_one(pool)
    .await
}

/// Remove a data-table link. Returns true if a link was removed.
pub async fn unlink_data_table(
    pool: &PgPool,
    table_id: i64,
    target_type: &str,
    target_id: i64,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "DELETE FROM data_table_links
          WHERE table_id = $1 AND target_type = $2 AND target_id = $3",
    )
    .bind(table_id)
    .bind(target_type)
    .bind(target_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// A data-table link row.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct DataTableLink {
    pub id: i64,
    pub table_id: i64,
    pub target_type: String,
    pub target_id: i64,
    pub role: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// All links for a given data table (surfaced by `data_table_describe`).
pub async fn links_for_table(
    pool: &PgPool,
    table_id: i64,
) -> Result<Vec<DataTableLink>, sqlx::Error> {
    sqlx::query_as::<_, DataTableLink>(
        "SELECT id, table_id, target_type, target_id, role, created_at
           FROM data_table_links
          WHERE table_id = $1
          ORDER BY created_at",
    )
    .bind(table_id)
    .fetch_all(pool)
    .await
}
