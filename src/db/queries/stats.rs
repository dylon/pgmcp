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
    let total = sqlx::query_scalar::<_, Option<i64>>("SELECT SUM(size_bytes) FROM indexed_files")
        .fetch_one(pool)
        .await?;
    Ok(total.unwrap_or(0) as u64)
}
