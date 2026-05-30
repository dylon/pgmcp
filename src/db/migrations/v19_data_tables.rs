//! Migration step 19: `data_tables_v1` — client-defined JSON data tables.
//!
//! Lets an MCP client create ad-hoc tables to record observations (benchmark
//! results, review findings, tracked metrics, …), then query / aggregate /
//! render them. The defining safety property is **no dynamic DDL**: a user
//! "table" is a *row* in `data_tables`, its optional typed schema is rows in
//! `data_table_columns`, and its observations are rows in `data_table_rows`
//! (each row's fields in one JSONB `data` column). A `data_table_create` is an
//! `INSERT`; a `data_table_drop` is a `DELETE` + FK CASCADE — no tool argument
//! ever reaches SQL as an identifier. See `docs/decisions/010-json-data-tables.md`.
//!
//! Three fixed tables:
//! - `data_tables`        — one row per user table (name unique per project /
//!   global; `schema_mode` ∈ open|strict; a `vector(1024)` embedding of
//!   name+description for semantic discovery).
//! - `data_table_columns` — 0..N typed columns; `data_type` CHECK is built from
//!   the closed [`crate::datatable::column_type::ColumnType`] enum (ADR-003).
//! - `data_table_rows`    — the observations; `data JSONB` with a GIN index for
//!   `@>` containment filters.
//!
//! The HNSW index on `data_tables.embedding` is built by
//! `ensure_data_tables_hnsw_index` (params-tracked rebuild), called
//! unconditionally after this step. Version-gated (runs once); every statement
//! is `IF NOT EXISTS` / idempotent.

use sqlx::PgPool;

pub(super) const DATA_TABLES_V1: i32 = 19;
pub(super) const DATA_TABLES_V1_NAME: &str = "data_tables_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // 1. data_tables — the table registry. `name` charset is CHECK-constrained
    //    (defense-in-depth; names are bound values, never SQL identifiers). The
    //    two partial unique indexes give "unique per project, and unique among
    //    the global (NULL project) scope" (a plain UNIQUE treats NULLs distinct).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS data_tables (
            id                  BIGSERIAL PRIMARY KEY,
            project_id          INTEGER REFERENCES projects(id) ON DELETE CASCADE,
            name                TEXT NOT NULL,
            description         TEXT,
            schema_mode         TEXT NOT NULL DEFAULT 'open'
                                  CHECK (schema_mode IN ('open','strict')),
            created_by          TEXT,
            embedding           vector(1024),
            embedding_signature TEXT NOT NULL DEFAULT 'data-table-v1',
            created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
            CHECK (name ~ '^[a-z][a-z0-9_]{0,62}$')
        )",
    )
    .execute(pool)
    .await?;
    for stmt in [
        "CREATE UNIQUE INDEX IF NOT EXISTS uq_data_tables_name_proj
            ON data_tables (project_id, name) WHERE project_id IS NOT NULL",
        "CREATE UNIQUE INDEX IF NOT EXISTS uq_data_tables_name_global
            ON data_tables (name) WHERE project_id IS NULL",
        "CREATE INDEX IF NOT EXISTS idx_data_tables_project
            ON data_tables (project_id)",
    ] {
        sqlx::query(stmt).execute(pool).await?;
    }

    // 2. data_table_columns — the optional typed schema. `data_type` vocabulary
    //    CHECK is reconciled from the closed ColumnType enum (ADR-003 idiom).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS data_table_columns (
            id            BIGSERIAL PRIMARY KEY,
            table_id      BIGINT NOT NULL REFERENCES data_tables(id) ON DELETE CASCADE,
            name          TEXT NOT NULL,
            data_type     TEXT NOT NULL,
            required      BOOLEAN NOT NULL DEFAULT FALSE,
            default_json  TEXT,
            position      INTEGER NOT NULL DEFAULT 0,
            description   TEXT,
            created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
            CHECK (name ~ '^[a-z][a-z0-9_]{0,62}$'),
            UNIQUE (table_id, name)
        )",
    )
    .execute(pool)
    .await?;
    super::v4_work_items::install_check(
        pool,
        "data_table_columns",
        "data_table_columns_type_check",
        &format!(
            "data_type IN ({})",
            crate::datatable::column_type::sql_in_list()
        ),
    )
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_data_table_columns_table
            ON data_table_columns (table_id, position)",
    )
    .execute(pool)
    .await?;

    // 3. data_table_rows — the observations. `data` holds the row's fields as one
    //    JSONB object; the GIN index (jsonb_path_ops, the smaller/faster opclass
    //    for `@>`) backs equality/containment filters.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS data_table_rows (
            id          BIGSERIAL PRIMARY KEY,
            table_id    BIGINT NOT NULL REFERENCES data_tables(id) ON DELETE CASCADE,
            data        JSONB NOT NULL DEFAULT '{}'::jsonb,
            created_by  TEXT,
            source      TEXT,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
    )
    .execute(pool)
    .await?;
    for stmt in [
        "CREATE INDEX IF NOT EXISTS idx_data_table_rows_table
            ON data_table_rows (table_id, id DESC)",
        "CREATE INDEX IF NOT EXISTS idx_data_table_rows_created
            ON data_table_rows (table_id, created_at DESC)",
        "CREATE INDEX IF NOT EXISTS idx_data_table_rows_data_gin
            ON data_table_rows USING GIN (data jsonb_path_ops)",
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
        // Pinning the constant — changing it is a schema-breaking event.
        assert_eq!(DATA_TABLES_V1, 19);
        assert_eq!(DATA_TABLES_V1_NAME, "data_tables_v1");
    }

    #[test]
    fn column_type_check_vocabulary_is_present() {
        // The CHECK predicate is built from the closed ColumnType enum, so the
        // migration and the validator share one source of truth.
        let list = crate::datatable::column_type::sql_in_list();
        for ty in [
            "'text'",
            "'integer'",
            "'number'",
            "'boolean'",
            "'timestamp'",
            "'json'",
        ] {
            assert!(list.contains(ty), "ColumnType CHECK missing {ty}");
        }
    }
}
