//! Migration step 64: `pgmcp_realtime_events_v1` — a small transactional event
//! log for the web UI and other local control-plane consumers.
//!
//! The table is append-only and intentionally generic: producers write the
//! affected topic/entity/op plus a compact JSON payload after mutating their
//! domain table. Consumers replay by `seq` and only read rows whose inserting
//! transaction is older than the current snapshot xmin, which prevents a stream
//! from observing `seq = 11`, advancing its cursor, and then missing a still
//! uncommitted lower sequence.
//!
//! A PostgreSQL `NOTIFY` trigger supports optional low-latency listeners;
//! websocket replay remains correct without relying on notifications.

use sqlx::PgPool;

pub(super) const REALTIME_EVENTS_V1: i32 = 64;
pub(super) const REALTIME_EVENTS_V1_NAME: &str = "pgmcp_realtime_events_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS pgmcp_realtime_events (
            seq         BIGSERIAL PRIMARY KEY,
            ins_xid     xid8        NOT NULL DEFAULT pg_current_xact_id(),
            topic       TEXT        NOT NULL,
            entity_kind TEXT        NOT NULL,
            entity_id   TEXT        NOT NULL,
            op          TEXT        NOT NULL,
            payload     JSONB       NOT NULL DEFAULT '{}'::jsonb,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
    )
    .execute(pool)
    .await?;

    for idx in [
        "CREATE INDEX IF NOT EXISTS idx_realtime_events_topic_seq \
            ON pgmcp_realtime_events(topic, seq)",
        "CREATE INDEX IF NOT EXISTS idx_realtime_events_entity_seq \
            ON pgmcp_realtime_events(entity_kind, entity_id, seq DESC)",
        "CREATE INDEX IF NOT EXISTS idx_realtime_events_created_at \
            ON pgmcp_realtime_events(created_at)",
    ] {
        sqlx::query(idx).execute(pool).await?;
    }

    // ADR-003: the CHECK vocabularies are built from the closed Rust enums'
    // `sql_in_list()` (the single source of truth), exactly as the work-item
    // kind/status/severity constraints are. Golden tests in
    // `crate::realtime::{topic,op}` pin the produced literal, so the enum and
    // this constraint can never drift.
    super::v4_work_items::install_check(
        pool,
        "pgmcp_realtime_events",
        "pgmcp_realtime_events_topic_check",
        &format!("topic IN ({})", crate::realtime::topic::sql_in_list()),
    )
    .await?;
    super::v4_work_items::install_check(
        pool,
        "pgmcp_realtime_events",
        "pgmcp_realtime_events_op_check",
        &format!("op IN ({})", crate::realtime::op::sql_in_list()),
    )
    .await?;

    sqlx::query(
        "CREATE OR REPLACE FUNCTION pgmcp_notify_realtime_event()
         RETURNS trigger
         LANGUAGE plpgsql
         AS $$
         BEGIN
             PERFORM pg_notify(
                 'pgmcp_realtime_events',
                 json_build_object('seq', NEW.seq, 'topic', NEW.topic)::text
             );
             RETURN NEW;
         END;
         $$",
    )
    .execute(pool)
    .await?;

    sqlx::query("DROP TRIGGER IF EXISTS trg_pgmcp_realtime_events_notify ON pgmcp_realtime_events")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE TRIGGER trg_pgmcp_realtime_events_notify
         AFTER INSERT ON pgmcp_realtime_events
         FOR EACH ROW EXECUTE FUNCTION pgmcp_notify_realtime_event()",
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
        assert_eq!(REALTIME_EVENTS_V1, 64);
        assert_eq!(REALTIME_EVENTS_V1_NAME, "pgmcp_realtime_events_v1");
    }
}
