//! Migration step 3: `cross_language_signature_clones_v1` — the
//! materialized table that powers `mcp__pgmcp__cross_language_api_equivalents`
//! and the cross-language section in `find_duplicates` /
//! `find_similar_modules` / `compare_files`.
//!
//! Pairs of `file_symbols` rows that share a `signature_shape_hash` and
//! come from files in different languages are inserted symmetric in the
//! key (with the lower symbol id stored as `symbol_id_a`). The
//! `similarity` column quantifies the match strength on a 0–1 scale.

use sqlx::PgPool;

/// Step version number — must be unique across all migration steps.
pub(super) const CROSS_LANGUAGE_SIGNATURES_V1: i32 = 3;
pub(super) const CROSS_LANGUAGE_SIGNATURES_V1_NAME: &str = "cross_language_signatures_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS cross_language_signature_clones (
            symbol_id_a BIGINT NOT NULL REFERENCES file_symbols(id) ON DELETE CASCADE,
            symbol_id_b BIGINT NOT NULL REFERENCES file_symbols(id) ON DELETE CASCADE,
            signature_shape_hash BIGINT NOT NULL,
            similarity REAL NOT NULL,
            language_a TEXT NOT NULL,
            language_b TEXT NOT NULL,
            project_id_a INT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            project_id_b INT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            CHECK (symbol_id_a < symbol_id_b),
            CHECK (similarity >= 0.0 AND similarity <= 1.0),
            PRIMARY KEY (symbol_id_a, symbol_id_b)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_cls_clones_symbol_a
            ON cross_language_signature_clones (symbol_id_a)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_cls_clones_symbol_b
            ON cross_language_signature_clones (symbol_id_b)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_cls_clones_hash
            ON cross_language_signature_clones (signature_shape_hash)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_cls_clones_similarity
            ON cross_language_signature_clones (similarity DESC)",
    )
    .execute(pool)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(CROSS_LANGUAGE_SIGNATURES_V1, 3);
        assert_eq!(
            CROSS_LANGUAGE_SIGNATURES_V1_NAME,
            "cross_language_signatures_v1"
        );
    }
}
