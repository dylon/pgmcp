//! Reactive file-event ingestion (ADR-022) — the single fan-in point where
//! **every** capture source converges, and the single batched writer that is the
//! sole producer of `client_file_events` rows.
//!
//! ## Why a stream
//!
//! The capture sources are heterogeneous and bursty: the `POST
//! /api/client/file_event` hook (one row per agent tool call), the eBPF
//! cgroup/PID probe (a `cargo build` `open()`s tens of thousands of files), and
//! the `/proc/<pid>/fd` liveness sampler. Inserting one row per event — as each
//! source did independently before — means a DB round-trip per `open()` under a
//! build flood. Funnelling them through one bounded [`Subject`] lets a small
//! operator chain collapse the burst and amortise the writes:
//!
//! ```text
//! producers ─try_send─▶ Subject ─▶ dedup_ttl ─▶ buffer_time_count ─▶ writer
//!  (hook / eBPF /        (bounded,   (1 row per     (≤N rows or       (resolve
//!   proc_fd)             drop-new)    actor·op·path   ≤T ms per        project/file
//!                                     per window)     batch)           once/path,
//!                                                                      multi-row INSERT)
//! ```
//!
//! Project/file resolution is deferred to the writer so it runs once per
//! *distinct path* **after** dedup has collapsed the burst — the main efficiency
//! lever over the old resolve-then-insert-per-event paths.
//!
//! ## Backpressure
//!
//! The [`Subject`] is a bounded crossbeam channel. Producers use `try_send`
//! (never `send`), so when the buffer is full the newest event is **dropped** —
//! attribution telemetry loss is acceptable (the indexer self-heals file content
//! separately), and a blocked producer (the HTTP hook, or the eBPF reader) is
//! not. Durability for the hook path is preserved upstream: the handler still
//! spools to the DB-down outbox before emitting, so a real outage replays.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crossbeam_channel::Sender;
use sqlx::PgPool;
use tracing::debug;

use crate::proc_clients::file_events::FileTouchEvent;
use crate::reactive::observable::Observable;
use crate::reactive::operators::{buffer_time_count, dedup_ttl};
use crate::reactive::subject::Subject;
use crate::reactive::subscription::Subscription;

/// Owns the running ingestion pipeline. Holding this keeps the writer
/// subscription (and therefore the whole operator chain) alive; dropping it
/// cancels the writer. The daemon stores it for its lifetime.
pub struct FileEventIngest {
    tx: Sender<FileTouchEvent>,
    _writer: Subscription,
}

impl FileEventIngest {
    /// Build the reactive pipeline + batched writer and return the handle.
    ///
    /// **Must be called from a tokio runtime thread**: it captures
    /// [`tokio::runtime::Handle::current`] so the writer (which runs on a plain
    /// std thread spawned by `Observable::subscribe`) can drive async sqlx
    /// inserts via `Handle::block_on` — the same bridge `indexer::event_processor`
    /// uses for its scanner DB calls.
    ///
    /// - `capacity` — Subject buffer; full ⇒ producers drop-newest.
    /// - `dedup_secs` — collapse identical `(actor, op, path)` within this window.
    /// - `batch_ms` / `batch_max` — flush a batch after this long, or this many.
    /// - `pg_notify_channel` — `Some(ch)` ⇒ the writer `pg_notify`s each landed
    ///   batch on `ch` for the live fan-out (ADR-022); `None` ⇒ no fan-out.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        pool: PgPool,
        capacity: usize,
        dedup_secs: u64,
        batch_ms: u64,
        batch_max: usize,
        pg_notify_channel: Option<String>,
        memory_pressure: std::sync::Arc<crate::health::MemoryPressure>,
    ) -> Self {
        let subject: Subject<FileTouchEvent> = Subject::new(capacity.max(1));
        let tx = subject.sender();
        let raw_rx = subject.receiver();

        // dedup (keyed throttle) → batch (time-or-count) → write.
        let deduped = dedup_ttl(raw_rx, Duration::from_secs(dedup_secs.max(1)), dedup_key);
        let batched = buffer_time_count(
            deduped,
            Duration::from_millis(batch_ms.max(1)),
            batch_max.max(1),
        );

        let rt = tokio::runtime::Handle::current();
        let notify_ch = pg_notify_channel;
        let writer =
            Observable::from_receiver(batched).subscribe(move |batch: Vec<FileTouchEvent>| {
                // Best-effort memory gate (src/health): under memory pressure, skip
                // this batch instead of driving its DB writes — the events are
                // drop-tolerant telemetry, and skipping keeps the ingest writer from
                // adding to a balloon. (The embed-intake gate + heavy-cron gate carry
                // the load-bearing pauses; this is the marginal ingest path.)
                if memory_pressure.is_paused() {
                    return;
                }
                // Plain std thread (subscribe spawns one), so block_on is legal and
                // drives the batch insert to completion before the next batch.
                rt.block_on(write_batch(&pool, batch, notify_ch.as_deref()));
            });

        // `subject` is dropped here, but the cloned `tx` (held by us + handed to
        // producers) and the operator threads' cloned receivers keep the channel
        // alive until daemon shutdown drops every sender.
        Self {
            tx,
            _writer: writer,
        }
    }

    /// A multi-producer sender for a capture source. Producers should `try_send`
    /// (drop-on-full) rather than `send` (block).
    pub fn sender(&self) -> Sender<FileTouchEvent> {
        self.tx.clone()
    }
}

/// Dedup identity: one row per `(cgroup, pid, session, op, path)` per window.
/// All actor handles are folded in so a hook event (session-keyed) and a
/// PID-native event (cgroup/pid-keyed) for the same path don't collapse together.
fn dedup_key(ev: &FileTouchEvent) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        ev.cgroup_id.unwrap_or(0),
        ev.pid.unwrap_or(0),
        ev.session_id.map(|u| u.to_string()).unwrap_or_default(),
        ev.op.as_str(),
        ev.abs_path,
    )
}

/// Resolve `indexed_files.id` for many paths in one query (NULL ⇒ unindexed/just
/// written; the row still records via `abs_path`).
async fn resolve_file_ids(pool: &PgPool, paths: &[String]) -> HashMap<String, i64> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT path, id FROM indexed_files WHERE path = ANY($1)")
            .bind(paths)
            .fetch_all(pool)
            .await
            .unwrap_or_default();
    rows.into_iter().collect()
}

/// Write one coalesced batch as a single multi-row INSERT. Resolves project
/// (longest-prefix cwd) and indexed-file id once per distinct path. On DB error
/// the batch is dropped with a debug log — this is best-effort telemetry, and
/// the hook path's durability is handled upstream by the outbox.
async fn write_batch(pool: &PgPool, batch: Vec<FileTouchEvent>, pg_notify_channel: Option<&str>) {
    if batch.is_empty() {
        return;
    }

    // Distinct paths (preserve first-seen order; bounded by batch_max).
    let mut distinct: Vec<String> = Vec::with_capacity(batch.len());
    let mut seen: HashSet<&str> = HashSet::with_capacity(batch.len());
    for ev in &batch {
        if seen.insert(ev.abs_path.as_str()) {
            distinct.push(ev.abs_path.clone());
        }
    }

    // One batched lookup for file ids; per-path longest-prefix for projects
    // (cached so duplicate paths in the same batch cost one query each).
    let file_ids = resolve_file_ids(pool, &distinct).await;
    let mut projects: HashMap<String, Option<i32>> = HashMap::with_capacity(distinct.len());
    for path in &distinct {
        // Lean id-only lookup (no correlated COUNT) — this loop runs per distinct
        // path every ~200 ms and only needs the id.
        let pid = crate::db::queries::find_project_id_by_cwd(pool, path)
            .await
            .ok()
            .flatten();
        projects.insert(path.clone(), pid);
    }

    // Multi-row INSERT (≤ batch_max rows × 12 cols ≪ the 65535 bind cap).
    let mut qb = sqlx::QueryBuilder::new(
        "INSERT INTO client_file_events \
         (mcp_session_id, session_id, pid, cgroup_id, root_pid, ppid, \
          file_id, project_id, abs_path, op, source, agent_id) ",
    );
    qb.push_values(batch.iter(), |mut b, ev| {
        b.push_bind(ev.mcp_session_id.clone())
            .push_bind(ev.session_id)
            .push_bind(ev.pid)
            // kernel u64 cgroup id → BIGINT i64 (inodes fit below 2^63).
            .push_bind(ev.cgroup_id.map(|c| c as i64))
            .push_bind(ev.root_pid)
            .push_bind(ev.ppid)
            .push_bind(file_ids.get(&ev.abs_path).copied())
            .push_bind(projects.get(&ev.abs_path).copied().flatten())
            .push_bind(ev.abs_path.clone())
            .push_bind(ev.op.as_str())
            .push_bind(ev.source.as_str())
            .push_bind(ev.agent_id.clone());
    });

    if let Err(e) = qb.build().execute(pool).await {
        debug!(
            error = %e,
            count = batch.len(),
            "ingest: client_file_events batch insert failed; dropping (telemetry)"
        );
        return;
    }

    // Realtime event (topic=client): a coalesced file-touch batch landed. Own-tx,
    // best-effort — compact rollup only (a single batch may span many sessions /
    // paths, so identity lives in the `client_file_events` rows, not here).
    crate::realtime::emit(
        pool,
        &crate::realtime::RealtimeEvent::client_activity(batch.len(), distinct.len()),
    )
    .await;

    // Live fan-out (ADR-022): signal external LISTENers (the pi orchestrator,
    // tooling) that a batch landed. A Postgres NOTIFY payload is capped at 8000
    // bytes, so a large batch sends a compact summary instead of the full list —
    // the SSE stream / a `client_file_events` query carries the detail.
    if let Some(ch) = pg_notify_channel {
        let full = serde_json::to_string(&batch).unwrap_or_default();
        let payload = if full.len() > 7000 {
            serde_json::json!({
                "truncated": true,
                "count": batch.len(),
                "note": "batch too large for NOTIFY; use GET /api/client/file_events/stream \
                         or query client_file_events",
            })
            .to_string()
        } else {
            full
        };
        if let Err(e) = sqlx::query("SELECT pg_notify($1, $2)")
            .bind(ch)
            .bind(&payload)
            .execute(pool)
            .await
        {
            debug!(error = %e, "ingest: pg_notify fan-out failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc_clients::file_events::{FileEventSource, FileOp};

    fn ev(source: FileEventSource, op: FileOp, path: &str) -> FileTouchEvent {
        FileTouchEvent {
            source,
            op,
            abs_path: path.to_string(),
            pid: None,
            ppid: None,
            root_pid: None,
            cgroup_id: None,
            mcp_session_id: None,
            session_id: None,
            agent_id: None,
        }
    }

    #[test]
    fn dedup_key_distinguishes_actor_op_and_path() {
        let a = ev(FileEventSource::EbpfCgroup, FileOp::Write, "/ws/a.rs");
        let mut b = ev(FileEventSource::EbpfCgroup, FileOp::Write, "/ws/a.rs");
        // Same key when actor/op/path match.
        assert_eq!(dedup_key(&a), dedup_key(&b));
        // Different path ⇒ different key.
        b.abs_path = "/ws/b.rs".to_string();
        assert_ne!(dedup_key(&a), dedup_key(&b));
        // Different cgroup ⇒ different key (two agents touching the same file).
        let mut c = ev(FileEventSource::EbpfCgroup, FileOp::Write, "/ws/a.rs");
        c.cgroup_id = Some(42);
        assert_ne!(dedup_key(&a), dedup_key(&c));
    }

    #[test]
    fn dedup_key_collapses_across_sources() {
        // An eBPF-cgroup event and a preload event for the SAME (cgroup, pid, op,
        // path) must share a dedup key, so when both capture mechanisms run they
        // collapse to one row — `source` is deliberately NOT part of the key.
        let mut e = ev(
            FileEventSource::EbpfCgroup,
            FileOp::Write,
            "/ws/target/x.rlib",
        );
        e.cgroup_id = Some(303106);
        e.pid = Some(4712);
        let mut p = ev(FileEventSource::Preload, FileOp::Write, "/ws/target/x.rlib");
        p.cgroup_id = Some(303106);
        p.pid = Some(4712);
        assert_eq!(dedup_key(&e), dedup_key(&p));
    }
}
