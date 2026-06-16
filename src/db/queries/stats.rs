//! Count-shaped statistics queries — extracted from `queries.rs` as part
//! of the D.2 god-file split.

use sqlx::PgPool;

// ============================================================================
// Statistics queries
// ============================================================================

/// Count total indexed files.
pub async fn count_indexed_files(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM indexed_files")
        .fetch_one(pool)
        .await?;
    Ok(count as u64)
}

/// Count total chunks.
pub async fn count_chunks(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM file_chunks")
        .fetch_one(pool)
        .await?;
    Ok(count as u64)
}

/// Count total projects.
pub async fn count_projects(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM projects")
        .fetch_one(pool)
        .await?;
    Ok(count as u64)
}

/// Get total bytes indexed.
pub async fn total_bytes_indexed(pool: &PgPool) -> Result<u64, sqlx::Error> {
    // `size_bytes` is `bigint`, and Postgres widens `SUM(bigint)` to `numeric`
    // (overflow-safe). `numeric` does NOT decode into `i64` (sqlx 0.8 and 0.9
    // alike reject it), so cast the aggregate back to `bigint` — the workspace
    // total is far below the i64 ceiling. Without the cast this query errors at
    // runtime and `index_stats` silently reports 0 bytes.
    let total =
        sqlx::query_scalar::<_, Option<i64>>("SELECT SUM(size_bytes)::bigint FROM indexed_files")
            .fetch_one(pool)
            .await?;
    Ok(total.unwrap_or(0) as u64)
}
