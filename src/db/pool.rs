//! Database connection pool management.

use sqlx::postgres::{PgPoolOptions, PgPool};

use crate::config::DatabaseConfig;

/// Create a connection pool from the database configuration.
pub async fn create_pool(config: &DatabaseConfig) -> Result<PgPool, sqlx::Error> {
    let pool = PgPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(&config.connection_url())
        .await?;

    Ok(pool)
}

/// Health check — run a simple query to verify connectivity.
pub async fn health_check(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT 1")
        .execute(pool)
        .await?;
    Ok(())
}
