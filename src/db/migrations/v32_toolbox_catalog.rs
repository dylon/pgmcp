//! Migration step 32: `toolbox_catalog_v1`.
//!
//! Creates the developer-tool "card" catalog — the formal-verification and
//! profiling/benchmarking/debugging tools installed on this machine — modeled
//! on the software-pattern catalog (`src/patterns/` → `software_patterns`).
//!
//! Two tables:
//!
//! - `tool_categories` — the seeded category reference table (the
//!   `programming_paradigms` analogue); every `tool_cards.category` references
//!   a slug here (enforced in Rust by
//!   `tools_catalog::tests::tools_reference_seeded_categories`).
//! - `tool_cards` — one compact row per tool, 1024d-direct (the
//!   `durable_mandates` / `data_tables` model): the `embedding vector(1024)`
//!   column is filled by the embedding-migration cron, NOT on the seed/write
//!   path. The `domain` CHECK is built from `ToolDomain::sql_in_list()` so the
//!   Rust enum is the single source of truth for the closed vocabulary (ADR-003;
//!   `tools_catalog::tests::domain_sql_in_list_is_pinned` pins the literal).
//!
//! The HNSW index on `tool_cards.embedding` is built unconditionally outside the
//! version gate (`ensure_tool_cards_hnsw_index` in `migrations.rs`), matching the
//! `data_tables` / v31 discipline so it exists on fresh and upgraded installs.
//!
//! Additive + `IF NOT EXISTS`, so idempotent and version-gated.

use sqlx::PgPool;

use crate::tools_catalog::ToolDomain;

pub(super) const TOOLBOX_CATALOG_V1: i32 = 32;
pub(super) const TOOLBOX_CATALOG_V1_NAME: &str = "toolbox_catalog_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS tool_categories (
            id          SERIAL PRIMARY KEY,
            slug        TEXT UNIQUE NOT NULL,
            name        TEXT NOT NULL,
            description TEXT NOT NULL,
            domain      TEXT NOT NULL,
            created_at  TIMESTAMPTZ DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;

    // Closed-vocabulary CHECK sourced from the Rust enum (ADR-003 idiom). The
    // interpolated value is enum-derived and trusted (no user input).
    let create_tool_cards = format!(
        "CREATE TABLE IF NOT EXISTS tool_cards (
            id                  BIGSERIAL PRIMARY KEY,
            slug                TEXT UNIQUE NOT NULL,
            name                TEXT NOT NULL,
            domain              TEXT NOT NULL CHECK (domain IN ({domains})),
            category            TEXT NOT NULL,
            summary             TEXT NOT NULL,
            what_it_does        TEXT NOT NULL,
            when_to_use         TEXT NOT NULL,
            inputs_outputs      TEXT NOT NULL,
            invocation          TEXT NOT NULL,
            strengths           TEXT NOT NULL,
            limitations         TEXT NOT NULL,
            alternatives        TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[],
            availability        TEXT NOT NULL,
            docs_url            TEXT,
            content_hash        BIGINT,
            embedding           vector(1024),
            embedding_signature TEXT DEFAULT 'bge-m3-v1',
            created_at          TIMESTAMPTZ DEFAULT NOW(),
            updated_at          TIMESTAMPTZ DEFAULT NOW()
        )",
        domains = ToolDomain::sql_in_list(),
    );
    sqlx::query(&create_tool_cards).execute(pool).await?;

    for stmt in [
        "CREATE INDEX IF NOT EXISTS idx_tool_categories_slug ON tool_categories(slug)",
        "CREATE INDEX IF NOT EXISTS idx_tool_cards_domain    ON tool_cards(domain)",
        "CREATE INDEX IF NOT EXISTS idx_tool_cards_category  ON tool_cards(category)",
        "CREATE INDEX IF NOT EXISTS idx_tool_cards_alts      ON tool_cards USING gin(alternatives)",
    ] {
        sqlx::query(stmt).execute(pool).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(TOOLBOX_CATALOG_V1, 32);
        assert_eq!(TOOLBOX_CATALOG_V1_NAME, "toolbox_catalog_v1");
    }
}
