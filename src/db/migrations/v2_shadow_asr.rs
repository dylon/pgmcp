//! Migration step 2: `shadow_asr_v1` — the unified semantic representation
//! schema additions documented in ADR-003 and the plan at
//! `~/.claude/plans/would-translating-the-asts-cosmic-quill.md`.
//!
//! Adds, in one transaction (or one logical sequence of idempotent
//! statements, depending on what each ALTER supports):
//!
//! 1. `type_tag_catalog` — open-set vocabulary of type tags (`int`,
//!    `container`, `mutex`, `metta_typed`, `linear`, …), seeded from
//!    `crate::parsing::type_tags::vocabulary::SEED_TYPE_TAGS`.
//! 2. `effect_catalog` — open-set vocabulary of effects (`async`,
//!    `unsafe`, `channel_send_persistent`, `term_rewrite`, …), seeded from
//!    `crate::parsing::type_tags::vocabulary::SEED_EFFECTS`.
//! 3. `symbol_parameters` — one row per function parameter with
//!    `(position, name, type_raw, type_tags, type_shape, default_value,
//!    modifier, is_variadic, is_self)`. GIN index on `type_tags`.
//! 4. `symbol_effects` — `(symbol_id, effect)` membership table.
//! 5. Additive columns on `file_symbols`: `return_type_raw`,
//!    `return_type_tags`, `return_type_shape`, `generic_params`,
//!    `scope_path`, `scope_depth`.
//! 6. Additive columns on `symbol_references`: `target_path`,
//!    `resolution_kind`, `resolution_confidence`.
//! 7. `pgmcp_metadata['shadow_asr_version'] = 1`.
//!
//! All ALTER COLUMN adds are nullable / defaulted so degraded reads during
//! the reindex window are safe (no dual-read code paths required).
//!
//! The migration validates that every catalog seed name belongs to the
//! Rust vocabulary at apply time, so a vocabulary edit that the developer
//! forgot to migrate cannot land. The persistence layer enforces the
//! reverse direction via CHECK constraints described below.

use sqlx::PgPool;

use crate::parsing::type_tags::vocabulary::{SEED_EFFECTS, SEED_TYPE_TAGS};

/// Step version number — must be unique across all migration steps.
pub(super) const SHADOW_ASR_V1: i32 = 2;
pub(super) const SHADOW_ASR_V1_NAME: &str = "shadow_asr_v1";

/// Apply the `shadow_asr_v1` migration step. Idempotent — safe to call on
/// installs where the step has already run; the runner gates it via
/// `version_applied`, but each statement is also written to be idempotent
/// (`IF NOT EXISTS`, `ON CONFLICT DO NOTHING`).
pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    create_catalog_tables(pool).await?;
    seed_catalog_tables(pool).await?;
    create_symbol_parameters_table(pool).await?;
    create_symbol_effects_table(pool).await?;
    extend_file_symbols(pool).await?;
    extend_symbol_references(pool).await?;
    stamp_metadata(pool).await?;
    Ok(())
}

async fn create_catalog_tables(pool: &PgPool) -> Result<(), sqlx::Error> {
    // Vocabulary catalog tables. Both have the same shape: name PK,
    // description, language_origin ('universal' | 'rust' | 'python' | ...).
    // Kept as separate tables (not a single `kind`-discriminated table)
    // because they have independent referential constraints:
    // `symbol_parameters.type_tags` ⊆ `type_tag_catalog`, and
    // `symbol_effects.effect` ⊆ `effect_catalog`. Mixing them would force
    // any reader to filter by `kind` which is purer noise than a join.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS type_tag_catalog (
            name TEXT PRIMARY KEY,
            description TEXT NOT NULL,
            language_origin TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS effect_catalog (
            name TEXT PRIMARY KEY,
            description TEXT NOT NULL,
            language_origin TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;

    Ok(())
}

async fn seed_catalog_tables(pool: &PgPool) -> Result<(), sqlx::Error> {
    // First-seed both catalogs from the Rust vocabulary. This runs only when
    // the v2 step is first applied to a database (the runner gates it via
    // `version_applied`), so vocabulary edits landed *after* v2 do NOT reach an
    // already-migrated DB from here. Ongoing catalog ⊇ vocabulary parity is
    // guaranteed instead by `super::reconcile_vocabulary_catalogs`, which runs
    // unconditionally on every boot (see that fn for the drift incident that
    // motivated it). `seed_catalog` is shared between the two paths.
    super::seed_catalog(pool, "type_tag_catalog", SEED_TYPE_TAGS).await?;
    super::seed_catalog(pool, "effect_catalog", SEED_EFFECTS).await?;
    Ok(())
}

async fn create_symbol_parameters_table(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS symbol_parameters (
            id BIGSERIAL PRIMARY KEY,
            symbol_id BIGINT NOT NULL REFERENCES file_symbols(id) ON DELETE CASCADE,
            position INT NOT NULL,
            name TEXT,
            type_raw TEXT,
            type_tags TEXT[] NOT NULL DEFAULT '{}',
            type_shape JSONB,
            default_value TEXT,
            modifier TEXT,
            is_variadic BOOLEAN NOT NULL DEFAULT FALSE,
            is_self BOOLEAN NOT NULL DEFAULT FALSE,
            UNIQUE (symbol_id, position)
        )",
    )
    .execute(pool)
    .await?;

    // CHECK constraint: every tag in type_tags must exist in type_tag_catalog.
    // Implemented via a trigger because PG can't directly array-subset CHECK
    // against another table.
    sqlx::query(
        "CREATE OR REPLACE FUNCTION validate_symbol_parameters_tags()
         RETURNS TRIGGER AS $$
         DECLARE
             bad TEXT;
         BEGIN
             SELECT t INTO bad
               FROM UNNEST(NEW.type_tags) AS t
              WHERE NOT EXISTS (
                    SELECT 1 FROM type_tag_catalog c WHERE c.name = t
              )
              LIMIT 1;
             IF bad IS NOT NULL THEN
                 RAISE EXCEPTION 'unknown type tag % on symbol_parameters', bad;
             END IF;
             RETURN NEW;
         END;
         $$ LANGUAGE plpgsql",
    )
    .execute(pool)
    .await?;
    sqlx::query("DROP TRIGGER IF EXISTS trg_symbol_parameters_tags ON symbol_parameters")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE TRIGGER trg_symbol_parameters_tags
            BEFORE INSERT OR UPDATE ON symbol_parameters
            FOR EACH ROW EXECUTE FUNCTION validate_symbol_parameters_tags()",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_symbol_parameters_symbol
            ON symbol_parameters (symbol_id)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_symbol_parameters_tags_gin
            ON symbol_parameters USING GIN (type_tags)",
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn create_symbol_effects_table(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS symbol_effects (
            symbol_id BIGINT NOT NULL REFERENCES file_symbols(id) ON DELETE CASCADE,
            effect TEXT NOT NULL REFERENCES effect_catalog(name) ON DELETE RESTRICT,
            PRIMARY KEY (symbol_id, effect)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_symbol_effects_effect ON symbol_effects (effect)")
        .execute(pool)
        .await?;
    Ok(())
}

async fn extend_file_symbols(pool: &PgPool) -> Result<(), sqlx::Error> {
    // All additive, all nullable — degraded reads during the reindex window
    // are safe.
    let statements = [
        "ALTER TABLE file_symbols ADD COLUMN IF NOT EXISTS return_type_raw TEXT",
        "ALTER TABLE file_symbols ADD COLUMN IF NOT EXISTS return_type_tags TEXT[] DEFAULT '{}'",
        "ALTER TABLE file_symbols ADD COLUMN IF NOT EXISTS return_type_shape JSONB",
        "ALTER TABLE file_symbols ADD COLUMN IF NOT EXISTS generic_params JSONB",
        "ALTER TABLE file_symbols ADD COLUMN IF NOT EXISTS scope_path TEXT",
        "ALTER TABLE file_symbols ADD COLUMN IF NOT EXISTS scope_depth INT",
    ];
    for stmt in statements {
        sqlx::query(stmt).execute(pool).await?;
    }

    // Validate tags on return_type_tags via trigger (same shape as the
    // symbol_parameters one).
    sqlx::query(
        "CREATE OR REPLACE FUNCTION validate_file_symbols_return_tags()
         RETURNS TRIGGER AS $$
         DECLARE
             bad TEXT;
         BEGIN
             IF NEW.return_type_tags IS NULL THEN
                 RETURN NEW;
             END IF;
             SELECT t INTO bad
               FROM UNNEST(NEW.return_type_tags) AS t
              WHERE NOT EXISTS (
                    SELECT 1 FROM type_tag_catalog c WHERE c.name = t
              )
              LIMIT 1;
             IF bad IS NOT NULL THEN
                 RAISE EXCEPTION 'unknown return type tag % on file_symbols', bad;
             END IF;
             RETURN NEW;
         END;
         $$ LANGUAGE plpgsql",
    )
    .execute(pool)
    .await?;
    sqlx::query("DROP TRIGGER IF EXISTS trg_file_symbols_return_tags ON file_symbols")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE TRIGGER trg_file_symbols_return_tags
            BEFORE INSERT OR UPDATE ON file_symbols
            FOR EACH ROW EXECUTE FUNCTION validate_file_symbols_return_tags()",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_file_symbols_return_tags_gin
            ON file_symbols USING GIN (return_type_tags)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_file_symbols_scope_path
            ON file_symbols (scope_path)",
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn extend_symbol_references(pool: &PgPool) -> Result<(), sqlx::Error> {
    let statements = [
        "ALTER TABLE symbol_references ADD COLUMN IF NOT EXISTS target_path TEXT",
        "ALTER TABLE symbol_references ADD COLUMN IF NOT EXISTS resolution_kind TEXT",
        "ALTER TABLE symbol_references ADD COLUMN IF NOT EXISTS resolution_confidence REAL",
    ];
    for stmt in statements {
        sqlx::query(stmt).execute(pool).await?;
    }

    sqlx::query(
        "ALTER TABLE symbol_references DROP CONSTRAINT IF EXISTS chk_symbol_refs_resolution_kind",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE symbol_references ADD CONSTRAINT chk_symbol_refs_resolution_kind
            CHECK (resolution_kind IS NULL OR resolution_kind IN (
                'exact_in_file', 'exact_via_import', 'bare_name_in_project',
                'external', 'unresolved'
            ))",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "ALTER TABLE symbol_references DROP CONSTRAINT IF EXISTS chk_symbol_refs_resolution_confidence",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE symbol_references ADD CONSTRAINT chk_symbol_refs_resolution_confidence
            CHECK (resolution_confidence IS NULL OR
                   (resolution_confidence >= 0.0 AND resolution_confidence <= 1.0))",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_symbol_references_target_path
            ON symbol_references (target_path)",
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn stamp_metadata(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value)
         VALUES ('shadow_asr_version', '1')
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsing::type_tags::vocabulary::{
        EFFECT_ASYNC, EFFECT_TERM_REWRITE, TAG_INT, TAG_LINEAR, TAG_METTA_TYPED,
    };

    #[test]
    fn step_version_is_stable() {
        // Pinning the constant — changing it is a schema-breaking event.
        assert_eq!(SHADOW_ASR_V1, 2);
        assert_eq!(SHADOW_ASR_V1_NAME, "shadow_asr_v1");
    }

    #[test]
    fn vocabulary_constants_resolve_for_migration_seed() {
        // Sanity: the constants the migration uses for its seed values
        // exist in the seed lists. If a developer renames a vocabulary
        // constant, this fails at unit-test time, well before the migration
        // would try to seed the catalog tables with a stale name.
        for tag in [TAG_INT, TAG_LINEAR, TAG_METTA_TYPED] {
            assert!(
                SEED_TYPE_TAGS.iter().any(|t| t.name == tag),
                "type tag {tag} missing from SEED_TYPE_TAGS"
            );
        }
        for effect in [EFFECT_ASYNC, EFFECT_TERM_REWRITE] {
            assert!(
                SEED_EFFECTS.iter().any(|t| t.name == effect),
                "effect {effect} missing from SEED_EFFECTS"
            );
        }
    }
}
