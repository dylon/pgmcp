//! The producer-side value type for one realtime event, plus one typed builder
//! per topic that owns that topic's compact payload JSON shape.
//!
//! A [`RealtimeEvent`] is a pure value: it does no IO. The [`super::emit`] seam
//! turns it into a `pgmcp_realtime_events` row (own-tx or in the caller's tx).
//! Keeping the payload shapes centralized here — rather than scattering
//! `serde_json::json!` literals across the ten chokepoints — means the web UI's
//! consumers have exactly one place to read the schema for each topic.
//!
//! Payloads are intentionally compact: identifiers, small enums, and counters
//! only — never file contents, embeddings, or other large blobs (the detail
//! lives in the domain tables the consumer can query by the ids carried here).

use serde_json::{Value, json};
use uuid::Uuid;

use super::op::Op;
use super::topic::Topic;

/// One realtime event, ready to be appended to `pgmcp_realtime_events`.
///
/// `entity_kind` is a `&'static str` (a small closed set of producer-owned
/// labels — `"work_item"`, `"mcp_client"`, …); `entity_id` is the per-entity
/// key a consumer uses to collapse successive upserts of the same entity.
pub struct RealtimeEvent {
    pub topic: Topic,
    pub entity_kind: &'static str,
    pub entity_id: String,
    pub op: Op,
    pub payload: Value,
}

impl RealtimeEvent {
    // -- tracker -----------------------------------------------------------

    /// A work-item / bug status transition (`set_work_item_status_in_tx`).
    #[allow(clippy::too_many_arguments)]
    pub fn tracker_status(
        public_id: &str,
        title: &str,
        from_status: &str,
        to_status: &str,
        actor: &str,
        kind: &str,
        project_id: Option<i32>,
    ) -> Self {
        Self {
            topic: Topic::Tracker,
            entity_kind: "work_item",
            entity_id: public_id.to_string(),
            op: Op::Upsert,
            payload: json!({
                "public_id": public_id,
                "title": title,
                "status": to_status,
                "from_status": from_status,
                "to_status": to_status,
                "actor": actor,
                "kind": kind,
                "project_id": project_id,
            }),
        }
    }

    /// A work-item mutation that is NOT a status transition — an operator field
    /// edit (title / priority / body / severity) or a bug-triage sidecar update
    /// from the admin console. Carries the item's *current* (unchanged) status
    /// so a consumer collapsing the `tracker` topic by `public_id` renders a
    /// snapshot consistent with the `tracker_status` upserts. `op = Upsert`.
    /// Emitted in the operator write's transaction (the field-update queries do
    /// not self-emit, unlike `set_work_item_status_in_tx`).
    pub fn tracker_update(
        public_id: &str,
        title: &str,
        status: &str,
        kind: &str,
        project_id: Option<i32>,
    ) -> Self {
        Self {
            topic: Topic::Tracker,
            entity_kind: "work_item",
            entity_id: public_id.to_string(),
            op: Op::Upsert,
            payload: json!({
                "public_id": public_id,
                "title": title,
                "status": status,
                "kind": kind,
                "project_id": project_id,
            }),
        }
    }

    // -- mandate -----------------------------------------------------------

    /// A session mandate promoted to durable scope, or a session-mandate
    /// upsert. `id` is the mandate row id; `scope` is `"session"` for a raw
    /// session upsert or the durable scope (`project` / `workspace` / …) on
    /// promotion.
    pub fn mandate_upsert(
        id: i64,
        scope: &str,
        polarity: &str,
        imperative: &str,
        target: Option<&str>,
    ) -> Self {
        Self {
            topic: Topic::Mandate,
            entity_kind: "mandate",
            entity_id: id.to_string(),
            op: Op::Upsert,
            payload: json!({
                "id": id,
                "scope": scope,
                "polarity": polarity,
                "imperative": imperative,
                "target": target,
            }),
        }
    }

    /// A session mandate retired (`retire_mandate`). Its sole caller,
    /// `crate::sessions::retire_mandate`, is itself `#[allow(dead_code)]` (the
    /// production hook path does not retire yet), so this builder carries the
    /// same allow to keep the separately-compiled binary target clean until the
    /// retire path is wired.
    #[allow(dead_code)]
    pub fn mandate_delete(id: i64, polarity: &str, imperative: &str) -> Self {
        Self {
            topic: Topic::Mandate,
            entity_kind: "mandate",
            entity_id: id.to_string(),
            op: Op::Delete,
            payload: json!({
                "id": id,
                "polarity": polarity,
                "imperative": imperative,
            }),
        }
    }

    // -- cron --------------------------------------------------------------

    /// One cron run tick, emitted alongside the persisted `cron_run_history`
    /// row.
    pub fn cron_tick(job: &str, outcome: &str, duration_ms: i64, trigger: &str) -> Self {
        Self {
            topic: Topic::Cron,
            entity_kind: "cron_job",
            entity_id: job.to_string(),
            op: Op::Tick,
            payload: json!({
                "job": job,
                "outcome": outcome,
                "duration_ms": duration_ms,
                "trigger": trigger,
            }),
        }
    }

    // -- index -------------------------------------------------------------

    /// An indexer batch-commit rollup for one workspace rescan. Batch-level,
    /// never per file. `files_submitted` is the combined added+updated count
    /// (the rescan path does not split the two); `chunk_count` is embedded
    /// asynchronously downstream and so is not known at this rollup point.
    pub fn index_snapshot(
        workspace: &str,
        total_scanned: u64,
        files_unchanged: u64,
        files_submitted: u64,
        files_deleted: u64,
        files_bounded_skipped: u64,
    ) -> Self {
        Self {
            topic: Topic::Index,
            entity_kind: "workspace",
            entity_id: workspace.to_string(),
            op: Op::Snapshot,
            payload: json!({
                "workspace": workspace,
                "total_scanned": total_scanned,
                "files_unchanged": files_unchanged,
                "files_submitted": files_submitted,
                "files_deleted": files_deleted,
                "files_bounded_skipped": files_bounded_skipped,
            }),
        }
    }

    // -- client ------------------------------------------------------------

    /// An MCP client connected / re-identified (`mcp_clients` upsert).
    pub fn client_upsert(mcp_session_id: &str, client_name: &str, project_id: Option<i32>) -> Self {
        Self {
            topic: Topic::Client,
            entity_kind: "mcp_client",
            entity_id: mcp_session_id.to_string(),
            op: Op::Upsert,
            payload: json!({
                "mcp_session_id": mcp_session_id,
                "client_name": client_name,
                "project_id": project_id,
            }),
        }
    }

    /// An MCP client observed exited by the liveness sweep.
    pub fn client_disconnect(mcp_session_id: &str, project_id: Option<i32>) -> Self {
        Self {
            topic: Topic::Client,
            entity_kind: "mcp_client",
            entity_id: mcp_session_id.to_string(),
            op: Op::Delete,
            payload: json!({
                "mcp_session_id": mcp_session_id,
                "project_id": project_id,
            }),
        }
    }

    /// A batch of client file-touch events landed. Compact rollup only (a
    /// single coalesced batch can span many sessions/paths); the detail is in
    /// `client_file_events`.
    pub fn client_activity(events: usize, distinct_paths: usize) -> Self {
        Self {
            topic: Topic::Client,
            entity_kind: "client_file_events",
            entity_id: "batch".to_string(),
            op: Op::Append,
            payload: json!({
                "events": events,
                "distinct_paths": distinct_paths,
            }),
        }
    }

    // -- scanner -----------------------------------------------------------

    /// An external-scanner findings batch was ingested.
    pub fn scanner_append(project: &str, scanner: &str, stored: u64, run_id: i64) -> Self {
        Self {
            topic: Topic::Scanner,
            entity_kind: "scanner_run",
            entity_id: run_id.to_string(),
            op: Op::Append,
            payload: json!({
                "project": project,
                "scanner": scanner,
                "stored": stored,
                "run_id": run_id,
            }),
        }
    }

    // -- control -----------------------------------------------------------

    /// A fleet-wide control action (all-stop halt / resume).
    pub fn control(halted: bool, reason: Option<&str>, actor: &str) -> Self {
        Self {
            topic: Topic::Control,
            entity_kind: "system_control",
            entity_id: "fleet".to_string(),
            op: Op::Tick,
            payload: json!({
                "halted": halted,
                "reason": reason,
                "actor": actor,
            }),
        }
    }

    // -- trace -------------------------------------------------------------

    /// A crucible trace span opened or closed. Gated to root spans by the
    /// caller (volume control).
    pub fn trace_append(trace_id: Uuid, span_id: i64, name: &str, status: &str) -> Self {
        Self {
            topic: Topic::Trace,
            entity_kind: "trace_span",
            entity_id: span_id.to_string(),
            op: Op::Append,
            payload: json!({
                "trace_id": trace_id.to_string(),
                "span_id": span_id,
                "name": name,
                "status": status,
            }),
        }
    }

    // -- task --------------------------------------------------------------

    /// An A2A task-state transition.
    pub fn task_upsert(task_id: Uuid, state: &str) -> Self {
        Self {
            topic: Topic::Task,
            entity_kind: "a2a_task",
            entity_id: task_id.to_string(),
            op: Op::Upsert,
            payload: json!({
                "task_id": task_id.to_string(),
                "state": state,
            }),
        }
    }

    // -- status ------------------------------------------------------------

    /// A periodic resource-usage snapshot from the sampler thread.
    pub fn status_snapshot(rss_bytes: u64, cpu_pct: f32, mem_used_bytes: u64) -> Self {
        Self {
            topic: Topic::Status,
            entity_kind: "resource_sample",
            entity_id: "daemon".to_string(),
            op: Op::Snapshot,
            payload: json!({
                "rss": rss_bytes,
                "cpu_pct": cpu_pct,
                "mem_used": mem_used_bytes,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_status_shape() {
        let ev = RealtimeEvent::tracker_status(
            "PLAN-1",
            "Do the thing",
            "in_progress",
            "claimed_done",
            "agent",
            "task",
            Some(7),
        );
        assert_eq!(ev.topic, Topic::Tracker);
        assert_eq!(ev.op, Op::Upsert);
        assert_eq!(ev.entity_kind, "work_item");
        assert_eq!(ev.entity_id, "PLAN-1");
        assert_eq!(ev.payload["to_status"], "claimed_done");
        assert_eq!(ev.payload["from_status"], "in_progress");
        assert_eq!(ev.payload["status"], "claimed_done");
        assert_eq!(ev.payload["project_id"], 7);
    }

    #[test]
    fn tracker_update_shape() {
        let ev = RealtimeEvent::tracker_update("PLAN-2", "Edit me", "in_progress", "task", Some(3));
        assert_eq!(ev.topic, Topic::Tracker);
        assert_eq!(ev.op, Op::Upsert);
        assert_eq!(ev.entity_kind, "work_item");
        assert_eq!(ev.entity_id, "PLAN-2");
        assert_eq!(ev.payload["status"], "in_progress");
        assert_eq!(ev.payload["title"], "Edit me");
        assert_eq!(ev.payload["project_id"], 3);
        // A non-status field edit must NOT invent from_status/to_status keys.
        assert!(ev.payload.get("from_status").is_none());
        assert!(ev.payload.get("to_status").is_none());
    }

    #[test]
    fn cron_tick_shape() {
        let ev = RealtimeEvent::cron_tick("graph-analysis", "ok", 1234, "scheduled");
        assert_eq!(ev.topic, Topic::Cron);
        assert_eq!(ev.op, Op::Tick);
        assert_eq!(ev.entity_id, "graph-analysis");
        assert_eq!(ev.payload["duration_ms"], 1234);
    }

    #[test]
    fn control_and_status_ops() {
        assert_eq!(
            RealtimeEvent::control(true, Some("ups"), "rest").op,
            Op::Tick
        );
        assert_eq!(
            RealtimeEvent::status_snapshot(1024, 12.5, 4096).op,
            Op::Snapshot
        );
        assert_eq!(
            RealtimeEvent::client_disconnect("sess-1", None).op,
            Op::Delete
        );
    }
}
