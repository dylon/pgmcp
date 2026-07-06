//! Write + operator-read queries for `durable_mandates` — the token-gated
//! web-UI admin console's create / edit / retire surface (ADR-034 amendment;
//! see `docs/decisions/034-webui-admin-console.md`).
//!
//! Every mutation is an `_in_tx` variant so the caller (the `src/api/
//! mandates_write.rs` handlers) can commit the domain change, its
//! `webui_audit_log` row (`crate::api::audit::audit_write_tx`), and the
//! `mandate` realtime event (`crate::realtime::emit_in_tx`) in ONE atomic
//! transaction — the ADR-021 in-tx posture: a failed audit/emit aborts the
//! mutation, so the audit trail and the realtime feed can never drift from the
//! durable table.
//!
//! [`DurableMandateRow`] carries the v67 operator-provenance / soft-delete
//! columns (`created_by` / `updated_at` / `retired_at`) that the leaner
//! `crate::sessions::DurableMandate` (the promotion read path) omits; the shared
//! [`DURABLE_MANDATE_COLS`] list pins the SELECT / RETURNING shape regardless of
//! physical column order.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};

/// A durable-mandate row including the v67 operator columns. Decoded from the
/// explicit [`DURABLE_MANDATE_COLS`] list on every read and `RETURNING`.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct DurableMandateRow {
    pub id: i64,
    pub scope: String,
    pub project_id: Option<i32>,
    pub polarity: String,
    pub imperative: String,
    pub target: Option<String>,
    pub source_mandate_id: Option<i64>,
    pub promoted_at: DateTime<Utc>,
    pub file_path: Option<String>,
    pub created_by: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
    pub retired_at: Option<DateTime<Utc>>,
}

/// Explicit column list (v65 base + v67 operator columns) shared by every
/// `SELECT` / `RETURNING` so [`DurableMandateRow`] decodes identically across
/// the create / read / update / retire paths.
const DURABLE_MANDATE_COLS: &str = "id, scope, project_id, polarity, imperative, target, \
     source_mandate_id, promoted_at, file_path, created_by, updated_at, retired_at";

/// Insert an operator-authored durable mandate (`source_mandate_id` NULL — it
/// did not originate from a session mandate) and return the created row. Stamps
/// `created_by` and `updated_at`. Runs in the caller's transaction so the audit
/// + realtime rows commit atomically with the insert.
pub async fn create_durable_mandate_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    scope: &str,
    project_id: Option<i32>,
    polarity: &str,
    imperative: &str,
    target: Option<&str>,
    created_by: &str,
) -> Result<DurableMandateRow, sqlx::Error> {
    sqlx::query_as::<_, DurableMandateRow>(sqlx::AssertSqlSafe(format!(
        "INSERT INTO durable_mandates
            (scope, project_id, polarity, imperative, target, source_mandate_id,
             created_by, updated_at)
         VALUES ($1, $2, $3, $4, $5, NULL, $6, NOW())
         RETURNING {DURABLE_MANDATE_COLS}"
    )))
    .bind(scope)
    .bind(project_id)
    .bind(polarity)
    .bind(imperative)
    .bind(target)
    .bind(created_by)
    .fetch_one(&mut **tx)
    .await
}

/// Fetch one durable mandate by id `FOR UPDATE`, locking the row and capturing
/// the pre-image the caller records in the audit `before`. `None` = no such row.
pub async fn get_durable_mandate_for_update_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: i64,
) -> Result<Option<DurableMandateRow>, sqlx::Error> {
    sqlx::query_as::<_, DurableMandateRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {DURABLE_MANDATE_COLS} FROM durable_mandates WHERE id = $1 FOR UPDATE"
    )))
    .bind(id)
    .fetch_optional(&mut **tx)
    .await
}

/// COALESCE-update the editable fields of a durable mandate (each `None` leaves
/// the stored value intact) and bump `updated_at`. Only affects a live
/// (non-retired) row; `None` = not found or already retired. Runs in the
/// caller's transaction.
pub async fn update_durable_mandate_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: i64,
    imperative: Option<&str>,
    target: Option<&str>,
    polarity: Option<&str>,
) -> Result<Option<DurableMandateRow>, sqlx::Error> {
    sqlx::query_as::<_, DurableMandateRow>(sqlx::AssertSqlSafe(format!(
        "UPDATE durable_mandates SET
            imperative = COALESCE($2, imperative),
            target = COALESCE($3, target),
            polarity = COALESCE($4, polarity),
            updated_at = NOW()
         WHERE id = $1 AND retired_at IS NULL
         RETURNING {DURABLE_MANDATE_COLS}"
    )))
    .bind(id)
    .bind(imperative)
    .bind(target)
    .bind(polarity)
    .fetch_optional(&mut **tx)
    .await
}

/// Soft-delete a durable mandate: stamp `retired_at = NOW()` (and `updated_at`).
/// Idempotent-guarded — only a currently-live row is affected, so `None` means
/// the mandate does not exist or was already retired (the handler maps that to
/// 404). Runs in the caller's transaction.
pub async fn retire_durable_mandate_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: i64,
) -> Result<Option<DurableMandateRow>, sqlx::Error> {
    sqlx::query_as::<_, DurableMandateRow>(sqlx::AssertSqlSafe(format!(
        "UPDATE durable_mandates SET retired_at = NOW(), updated_at = NOW()
         WHERE id = $1 AND retired_at IS NULL
         RETURNING {DURABLE_MANDATE_COLS}"
    )))
    .bind(id)
    .fetch_optional(&mut **tx)
    .await
}

/// List active (non-retired) durable mandates, newest-promoted first, optionally
/// filtered by `scope` and/or `project_id`. Backs the DB-backed leg of the
/// merged `GET /api/mandates` read (the admin console's mandate table).
pub async fn list_active_durable_mandates(
    pool: &PgPool,
    scope: Option<&str>,
    project_id: Option<i32>,
) -> Result<Vec<DurableMandateRow>, sqlx::Error> {
    sqlx::query_as::<_, DurableMandateRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {DURABLE_MANDATE_COLS} FROM durable_mandates
         WHERE retired_at IS NULL
           AND ($1::text IS NULL OR scope = $1)
           AND ($2::int  IS NULL OR project_id = $2)
         ORDER BY promoted_at DESC"
    )))
    .bind(scope)
    .bind(project_id)
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_list_covers_v65_base_and_v67_operator_columns() {
        // Guard against the SELECT/RETURNING shape drifting from the struct: the
        // v67 operator columns (created_by/updated_at/retired_at) MUST be present
        // so `retired_at IS NULL` filtering and provenance rendering work.
        for col in [
            "id",
            "scope",
            "project_id",
            "polarity",
            "imperative",
            "target",
            "source_mandate_id",
            "promoted_at",
            "file_path",
            "created_by",
            "updated_at",
            "retired_at",
        ] {
            assert!(
                DURABLE_MANDATE_COLS.contains(col),
                "DURABLE_MANDATE_COLS missing '{col}'"
            );
        }
    }
}
