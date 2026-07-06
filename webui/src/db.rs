use chrono::{DateTime, Utc};
use sqlx::PgPool;

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

pub async fn current_seq(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>("SELECT COALESCE(MAX(seq), 0) FROM pgmcp_realtime_events")
        .fetch_one(pool)
        .await
}

pub async fn committed_events_after(
    pool: &PgPool,
    after_seq: i64,
    limit: i64,
    topics: &[String],
) -> Result<Vec<RealtimeEventRow>, sqlx::Error> {
    let limit = limit.clamp(1, 2_000);
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
