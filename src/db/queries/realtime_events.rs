//! Queries for the transactional realtime event log used by the web UI.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

const DEFAULT_REPLAY_LIMIT: i64 = 250;
const MAX_REPLAY_LIMIT: i64 = 2_000;

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct RealtimeEventRow {
    pub seq: i64,
    pub topic: String,
    pub entity_kind: String,
    pub entity_id: String,
    pub op: String,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

pub fn clamp_realtime_replay_limit(limit: i64) -> i64 {
    if limit <= 0 {
        DEFAULT_REPLAY_LIMIT
    } else {
        limit.min(MAX_REPLAY_LIMIT)
    }
}

pub async fn append_realtime_event(
    pool: &PgPool,
    topic: &str,
    entity_kind: &str,
    entity_id: &str,
    op: &str,
    payload: &serde_json::Value,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO pgmcp_realtime_events
            (topic, entity_kind, entity_id, op, payload)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING seq",
    )
    .bind(topic)
    .bind(entity_kind)
    .bind(entity_id)
    .bind(op)
    .bind(payload)
    .fetch_one(pool)
    .await
}

/// In-transaction sibling of [`append_realtime_event`]. Binds the INSERT to the
/// caller's transaction so the row's `ins_xid` (defaulted to
/// `pg_current_xact_id()`) is the *mutation's* transaction id — the
/// xid-watermark visibility contract in [`committed_realtime_events_after`]
/// then guarantees a consumer never observes the event before the mutation it
/// describes has committed. Propagates errors so a lost event aborts the
/// mutation's transaction (they commit atomically or not at all).
pub async fn append_realtime_event_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    topic: &str,
    entity_kind: &str,
    entity_id: &str,
    op: &str,
    payload: &serde_json::Value,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO pgmcp_realtime_events
            (topic, entity_kind, entity_id, op, payload)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING seq",
    )
    .bind(topic)
    .bind(entity_kind)
    .bind(entity_id)
    .bind(op)
    .bind(payload)
    .fetch_one(&mut **tx)
    .await
}

pub async fn current_realtime_seq(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>("SELECT COALESCE(MAX(seq), 0) FROM pgmcp_realtime_events")
        .fetch_one(pool)
        .await
}

pub async fn committed_realtime_events_after(
    pool: &PgPool,
    after_seq: i64,
    limit: i64,
    topics: &[String],
) -> Result<Vec<RealtimeEventRow>, sqlx::Error> {
    let limit = clamp_realtime_replay_limit(limit);
    if topics.is_empty() {
        sqlx::query_as::<_, RealtimeEventRow>(
            "SELECT seq, topic, entity_kind, entity_id, op, payload, created_at
               FROM pgmcp_realtime_events
              WHERE seq > $1
                AND ins_xid < pg_snapshot_xmin(pg_current_snapshot())
              ORDER BY seq
              LIMIT $2",
        )
        .bind(after_seq.max(0))
        .bind(limit)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_as::<_, RealtimeEventRow>(
            "SELECT seq, topic, entity_kind, entity_id, op, payload, created_at
               FROM pgmcp_realtime_events
              WHERE seq > $1
                AND topic = ANY($3)
                AND ins_xid < pg_snapshot_xmin(pg_current_snapshot())
              ORDER BY seq
              LIMIT $2",
        )
        .bind(after_seq.max(0))
        .bind(limit)
        .bind(topics)
        .fetch_all(pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_limit_is_clamped() {
        assert_eq!(clamp_realtime_replay_limit(0), DEFAULT_REPLAY_LIMIT);
        assert_eq!(clamp_realtime_replay_limit(-10), DEFAULT_REPLAY_LIMIT);
        assert_eq!(clamp_realtime_replay_limit(42), 42);
        assert_eq!(clamp_realtime_replay_limit(20_000), MAX_REPLAY_LIMIT);
    }
}
