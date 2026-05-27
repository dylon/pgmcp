//! Lightweight schema-introspection helpers shared across migration steps.
//!
//! These make legacy-column DDL tolerant of the post-cutover
//! schema. The pre-BGE-M3 384-dim `embedding` column is permanently dropped at
//! cutover, but `ALTER TABLE … ALTER COLUMN embedding DROP NOT NULL` (and
//! `CREATE INDEX … (embedding …)`) have no `IF EXISTS` form — so callers gate
//! the legacy DDL on [`column_exists`] to keep `run_migrations` idempotent
//! across all three states (fresh / mid-migration dual-write / post-cutover).

use sqlx::PgPool;

/// True if `column` exists on `table` in the current database's `public`
/// schema.
pub(super) async fn column_exists(
    pool: &PgPool,
    table: &str,
    column: &str,
) -> Result<bool, sqlx::Error> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM information_schema.columns
             WHERE table_schema = 'public' AND table_name = $1 AND column_name = $2
         )",
    )
    .bind(table)
    .bind(column)
    .fetch_one(pool)
    .await?;
    Ok(exists)
}
