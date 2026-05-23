//! Cross-language signature equivalence reads from a materialized
//! `cross_language_signature_clones` table.
//!
//! The table is populated by `src/cron/cross_language_signatures.rs`
//! (Phase D2c) and stores pairs of symbols across different language
//! files that share `signature_shape_hash`. Tools query this helper to
//! surface "Python `def authenticate(user, password) -> Token` ≈ Rust
//! `fn authenticate(user, password) -> Token`" matches.
//!
//! This file ships the reader API and a `MaterializedAvailable` probe
//! so consumers can degrade gracefully when the cron hasn't yet run.

use sqlx::PgPool;

#[derive(Debug, Clone)]
pub struct EquivMatch {
    pub other_symbol_id: i64,
    pub other_file_id: i64,
    pub other_language: String,
    pub other_name: String,
    pub other_scope_path: Option<String>,
    pub similarity: f32,
}

/// Returns true when the `cross_language_signature_clones` table exists
/// AND has any rows. Tools that JOIN against the table can call this to
/// decide whether to surface a cross-language section in their response.
pub async fn materialized_available(pool: &PgPool) -> Result<bool, sqlx::Error> {
    let exists: Option<bool> = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM pg_tables
              WHERE schemaname = 'public'
                AND tablename = 'cross_language_signature_clones'
         )",
    )
    .fetch_one(pool)
    .await?;
    if !exists.unwrap_or(false) {
        return Ok(false);
    }
    let any: Option<bool> =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM cross_language_signature_clones LIMIT 1)")
            .fetch_optional(pool)
            .await?
            .flatten();
    Ok(any.unwrap_or(false))
}

/// Find cross-language equivalents for `symbol_id`. Returns an empty Vec
/// when the materialized table is absent or has no matches.
pub async fn equivalent_signatures(
    pool: &PgPool,
    symbol_id: i64,
    min_similarity: f32,
    limit: i64,
) -> Result<Vec<EquivMatch>, sqlx::Error> {
    if !materialized_available(pool).await? {
        return Ok(Vec::new());
    }
    let rows: Vec<(i64, i64, String, String, Option<String>, f32)> = sqlx::query_as(
        "SELECT fs.id, fs.file_id, f.language, fs.name, fs.scope_path, c.similarity
         FROM cross_language_signature_clones c
         JOIN file_symbols fs ON fs.id = CASE
             WHEN c.symbol_id_a = $1 THEN c.symbol_id_b
             ELSE c.symbol_id_a
         END
         JOIN indexed_files f ON f.id = fs.file_id
         WHERE (c.symbol_id_a = $1 OR c.symbol_id_b = $1)
           AND c.similarity >= $2::real
         ORDER BY c.similarity DESC
         LIMIT $3::int8",
    )
    .bind(symbol_id)
    .bind(min_similarity)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                other_symbol_id,
                other_file_id,
                other_language,
                other_name,
                other_scope_path,
                similarity,
            )| EquivMatch {
                other_symbol_id,
                other_file_id,
                other_language,
                other_name,
                other_scope_path,
                similarity,
            },
        )
        .collect())
}
