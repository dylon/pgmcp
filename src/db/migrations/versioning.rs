//! `pgmcp_schema_versions` helpers — extracted from `migrations.rs` as
//! part of the D.2 god-file split.
//!
//! These three helpers are called by `run_migrations` (and only by
//! `run_migrations`) to gate every numbered migration step. Keeping them
//! in a sibling submodule means future incremental migrations can plug
//! their per-step bodies into `migrations/v<NN>_<name>.rs` and call into
//! the same versioning helpers without growing the parent file.

use sqlx::PgPool;

/// `version_applied` / `record_version` call.
pub(super) async fn ensure_schema_versions_table(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS pgmcp_schema_versions (
            version INT PRIMARY KEY,
            name TEXT NOT NULL,
            applied_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Has the numbered migration step `version` been recorded?
pub(super) async fn version_applied(pool: &PgPool, version: i32) -> Result<bool, sqlx::Error> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pgmcp_schema_versions WHERE version = $1")
            .bind(version)
            .fetch_one(pool)
            .await?;
    Ok(count > 0)
}

/// Record successful completion of a numbered migration step. Idempotent
/// via `ON CONFLICT DO NOTHING`.
pub(super) async fn record_version(
    pool: &PgPool,
    version: i32,
    name: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pgmcp_schema_versions (version, name) VALUES ($1, $2)
         ON CONFLICT (version) DO NOTHING",
    )
    .bind(version)
    .bind(name)
    .execute(pool)
    .await?;
    Ok(())
}
