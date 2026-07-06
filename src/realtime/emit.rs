//! The emit seam: turn a [`RealtimeEvent`] into a `pgmcp_realtime_events` row.
//!
//! Two async entry points with deliberately different failure semantics:
//!
//! - [`emit`] runs in its **own** transaction. A realtime event is telemetry;
//!   it must never take down the mutation it is describing. So an insert error
//!   is caught and logged at `error!` (ADR-021: a swallowed/degraded runtime
//!   failure logs at `error!`, not `warn!`) and otherwise ignored.
//! - [`emit_in_tx`] runs **inside the caller's** transaction and propagates its
//!   error. Use it where the event must commit atomically with the mutation:
//!   the row's `ins_xid` is then the mutation's transaction id, satisfying the
//!   xid-watermark visibility contract in `committed_realtime_events_after`
//!   (a consumer never sees the event before the mutation it describes is
//!   visible). A lost event therefore aborts the mutation's tx — correct, since
//!   the two are meant to be one atomic fact.
//!
//! [`RealtimeEmitter`] bridges a **synchronous** (non-async) producer — e.g.
//! the std::thread resource sampler — onto the tokio runtime without blocking
//! it: `spawn` is fire-and-forget over [`emit`].

use sqlx::{PgPool, Postgres, Transaction};
use tracing::error;

use super::event::RealtimeEvent;

/// Append `ev` in its own transaction. Best-effort: on error, log at `error!`
/// and swallow — the caller's work must not be affected by a telemetry write.
pub async fn emit(pool: &PgPool, ev: &RealtimeEvent) {
    if let Err(e) = crate::db::queries::append_realtime_event(
        pool,
        ev.topic.as_str(),
        ev.entity_kind,
        &ev.entity_id,
        ev.op.as_str(),
        &ev.payload,
    )
    .await
    {
        error!(
            topic = ev.topic.as_str(),
            entity_kind = ev.entity_kind,
            entity_id = %ev.entity_id,
            op = ev.op.as_str(),
            error = %e,
            "realtime: append_realtime_event failed (own-tx); event dropped"
        );
    }
}

/// Append `ev` inside the caller's transaction, propagating any error so a lost
/// event aborts the mutation it accompanies. Returns the new `seq`.
pub async fn emit_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    ev: &RealtimeEvent,
) -> Result<i64, sqlx::Error> {
    crate::db::queries::append_realtime_event_tx(
        tx,
        ev.topic.as_str(),
        ev.entity_kind,
        &ev.entity_id,
        ev.op.as_str(),
        &ev.payload,
    )
    .await
}

/// A cloneable handle that lets a synchronous producer enqueue a realtime event
/// onto the tokio runtime without blocking. Cheap to clone (a `PgPool` handle
/// plus a runtime `Handle`). Used by the resource sampler (`src/stats/resources.rs`),
/// which runs on a plain std thread and so cannot `.await` directly.
#[derive(Clone)]
pub struct RealtimeEmitter {
    pool: PgPool,
    rt: tokio::runtime::Handle,
}

impl RealtimeEmitter {
    pub fn new(pool: PgPool, rt: tokio::runtime::Handle) -> Self {
        Self { pool, rt }
    }

    /// Fire-and-forget: spawn the own-tx [`emit`] on the runtime. Never blocks
    /// the calling (sync) thread; emit errors are logged by `emit` at `error!`.
    pub fn spawn(&self, ev: RealtimeEvent) {
        let pool = self.pool.clone();
        let rt = self.rt.clone();
        rt.spawn(async move {
            emit(&pool, &ev).await;
        });
    }
}
