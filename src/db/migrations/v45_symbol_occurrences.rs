//! Migration step 45: `symbol_occurrences` — a token-level identifier-occurrence
//! index (ADR-024).
//!
//! Where `file_symbols` records *definitions*, this records EVERY identifier
//! occurrence with **column offsets** (the missing LSP prerequisite) and a
//! closed `occurrence_kind` distinguishing an identifier in code
//! (`definition`/`code_reference`) from one in COMMENTARY (`comment`/`doc`) or a
//! string literal (`string`) — the user's "differentiate `x` in source from `x`
//! in commentary" requirement. `type_tags` carries the binder's coarse type
//! where the grammar exposes an explicit annotation (the "`x` as int vs string"
//! requirement); `enclosing_symbol_id` + `file_symbols.scope_path` answer
//! lexical-scoped occurrence queries.
//!
//! Volume note (ADR-024): at "all identifiers" fidelity this is the largest
//! table in the schema. It starts as one table with targeted indexes (BRIN on
//! the file-ordered `file_id`, btree on `name`, partial on definitions, GIN on
//! `type_tags`); if row count / index bloat exceeds budget after a full
//! extraction it migrates to `HASH(file_id)` declarative partitioning (the
//! decision is data-driven, benchmarked, not premature). Additive, idempotent.

use sqlx::PgPool;

use crate::parsing::occurrence_kind::OccurrenceKind;

pub(super) const SYMBOL_OCCURRENCES: i32 = 45;
pub(super) const SYMBOL_OCCURRENCES_NAME: &str = "symbol_occurrences";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS symbol_occurrences (
            id                  BIGSERIAL PRIMARY KEY,
            file_id             BIGINT  NOT NULL REFERENCES indexed_files(id) ON DELETE CASCADE,
            name                TEXT    NOT NULL,
            start_line          INTEGER NOT NULL,
            start_col           INTEGER NOT NULL,
            end_col             INTEGER NOT NULL,
            occurrence_kind     TEXT    NOT NULL,
            enclosing_symbol_id BIGINT  REFERENCES file_symbols(id) ON DELETE SET NULL,
            resolved_target_id  BIGINT  REFERENCES file_symbols(id) ON DELETE SET NULL,
            type_tags           TEXT[]  NOT NULL DEFAULT '{}'
        )",
    )
    .execute(pool)
    .await?;

    for idx in [
        // file-ordered inserts → BRIN is tiny and ideal for per-file lookups.
        "CREATE INDEX IF NOT EXISTS ix_symbol_occ_file ON symbol_occurrences USING brin (file_id)",
        // cursor lookup (file + line) and workspace-symbol-by-name.
        "CREATE INDEX IF NOT EXISTS ix_symbol_occ_file_line ON symbol_occurrences (file_id, start_line)",
        "CREATE INDEX IF NOT EXISTS ix_symbol_occ_name ON symbol_occurrences (name)",
        // definitions are the hot subset for go-to-definition.
        "CREATE INDEX IF NOT EXISTS ix_symbol_occ_def ON symbol_occurrences (name) WHERE occurrence_kind = 'definition'",
        "CREATE INDEX IF NOT EXISTS ix_symbol_occ_enclosing ON symbol_occurrences (enclosing_symbol_id) WHERE enclosing_symbol_id IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS ix_symbol_occ_type_tags ON symbol_occurrences USING gin (type_tags)",
    ] {
        sqlx::query(idx).execute(pool).await?;
    }

    super::v4_work_items::install_check(
        pool,
        "symbol_occurrences",
        "symbol_occurrences_kind_check",
        &format!("occurrence_kind IN ({})", OccurrenceKind::sql_in_list()),
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(SYMBOL_OCCURRENCES, 45);
        assert_eq!(SYMBOL_OCCURRENCES_NAME, "symbol_occurrences");
    }
}
