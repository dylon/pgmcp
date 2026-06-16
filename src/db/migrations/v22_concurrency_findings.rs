//! Migration step 22: concurrency persistence + temporal.
//!
//! Three tables backing the concurrency-scan cron and the temporal graph-RAG
//! integration (Layer 4):
//!
//! - `concurrency_findings` — append-with-upsert ledger of deadlock / bottleneck
//!   findings (provenance-keyed for idempotency; mirrors the v15 effect-drift
//!   ledger). `finding_kind` / `severity` are closed vocabularies (ADR-003):
//!   `TEXT` + a `CHECK` built from
//!   [`crate::concurrency::findings`]`::sql_in_list()` and
//!   [`crate::tracker::severity`]`::sql_in_list()`.
//! - `lock_order_edges` — bitemporal materialization of the interprocedural
//!   lock-order graph, so the unified-graph `lock_order` arm (Layer 4) is
//!   `as_of`-queryable and cycle regressions are visible. A partial unique index
//!   keeps at most one OPEN (`valid_to IS NULL`) row per edge; the cron closes
//!   edges it no longer sees and opens new rows when an edge reappears.
//! - `concurrency_health_history` — per-project snapshots (mirrors
//!   `quality_report_history`, v9) feeding the forecast / trajectory machinery.

use sqlx::PgPool;

use crate::concurrency::findings::sql_in_list as finding_kind_sql_in_list;
use crate::tracker::severity::sql_in_list as severity_sql_in_list;

pub(super) const CONCURRENCY_FINDINGS_V1: i32 = 22;
pub(super) const CONCURRENCY_FINDINGS_V1_NAME: &str = "concurrency_findings_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = 0")
        .execute(&mut *tx)
        .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS concurrency_findings (
            id                BIGSERIAL PRIMARY KEY,
            project_id        INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            finding_kind      TEXT NOT NULL,
            severity          TEXT NOT NULL,
            confidence        REAL NOT NULL DEFAULT 0.0,
            provenance_key    TEXT NOT NULL UNIQUE,
            symbol_id         BIGINT REFERENCES file_symbols(id) ON DELETE SET NULL,
            file_id           BIGINT REFERENCES indexed_files(id) ON DELETE SET NULL,
            evidence          JSONB NOT NULL DEFAULT '{}'::jsonb,
            title             TEXT NOT NULL,
            first_observed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            observed_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
            promoted_item_id  BIGINT
        )",
    )
    .execute(&mut *tx)
    .await?;

    for (name, col, list) in [
        (
            "chk_concurrency_findings_kind",
            "finding_kind",
            finding_kind_sql_in_list(),
        ),
        (
            "chk_concurrency_findings_severity",
            "severity",
            severity_sql_in_list(),
        ),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE concurrency_findings DROP CONSTRAINT IF EXISTS {name}"
        )))
        .execute(&mut *tx)
        .await?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE concurrency_findings ADD CONSTRAINT {name} CHECK ({col} IN ({list}))"
        )))
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS lock_order_edges (
            id              BIGSERIAL PRIMARY KEY,
            project_id      INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            from_key        TEXT NOT NULL,
            to_key          TEXT NOT NULL,
            from_mode       TEXT,
            to_mode         TEXT,
            min_confidence  REAL NOT NULL DEFAULT 0.0,
            interprocedural BOOLEAN NOT NULL DEFAULT FALSE,
            valid_from      TIMESTAMPTZ NOT NULL DEFAULT now(),
            valid_to        TIMESTAMPTZ,
            last_seen_at    TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS concurrency_health_history (
            id                   BIGSERIAL PRIMARY KEY,
            project_id           INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            computed_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
            deadlock_cycle_count INTEGER NOT NULL DEFAULT 0,
            channel_cycle_count  INTEGER NOT NULL DEFAULT 0,
            blocked_recv_count   INTEGER NOT NULL DEFAULT 0,
            max_lock_contention  REAL NOT NULL DEFAULT 0.0,
            raw_summary          JSONB NOT NULL DEFAULT '{}'::jsonb
        )",
    )
    .execute(&mut *tx)
    .await?;

    for stmt in [
        "CREATE INDEX IF NOT EXISTS idx_concurrency_findings_project
            ON concurrency_findings (project_id, finding_kind, severity)",
        "CREATE INDEX IF NOT EXISTS idx_concurrency_findings_observed
            ON concurrency_findings (observed_at DESC)",
        // At most one OPEN row per edge; supports the cron's upsert/close cycle.
        "CREATE UNIQUE INDEX IF NOT EXISTS uq_lock_order_edges_open
            ON lock_order_edges (project_id, from_key, to_key) WHERE valid_to IS NULL",
        "CREATE INDEX IF NOT EXISTS idx_lock_order_edges_project
            ON lock_order_edges (project_id, valid_to)",
        "CREATE INDEX IF NOT EXISTS idx_concurrency_health_project
            ON concurrency_health_history (project_id, computed_at DESC)",
    ] {
        sqlx::query(stmt).execute(&mut *tx).await?;
    }

    // Widen the work_item_finding_provenance source CHECK to the current
    // FindingSource set (v17 installed it once from the old 2-value list; the
    // `deadlock_cycle` / `channel_deadlock` sources were added for ADR-011
    // promotion). Idempotent DROP+ADD so fresh and upgraded installs converge.
    sqlx::query(
        "ALTER TABLE work_item_finding_provenance
         DROP CONSTRAINT IF EXISTS work_item_finding_provenance_source_check",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "ALTER TABLE work_item_finding_provenance
         ADD CONSTRAINT work_item_finding_provenance_source_check
         CHECK (finding_source IN ({}))",
        crate::tracker::git_link::finding_source_sql_in_list()
    )))
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(CONCURRENCY_FINDINGS_V1, 22);
        assert_eq!(CONCURRENCY_FINDINGS_V1_NAME, "concurrency_findings_v1");
    }

    #[test]
    fn checks_quote_the_vocabulary() {
        assert!(finding_kind_sql_in_list().contains("'deadlock_cycle'"));
        assert!(finding_kind_sql_in_list().contains("'lock_contention'"));
        assert!(severity_sql_in_list().contains("'critical'"));
    }
}
