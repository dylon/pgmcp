//! Parameter structs for the `crucible_trace_*` tools (ADR-020 E10 — unified run
//! tracing). UUIDs are accepted as strings and parsed in the tool bodies (so the
//! params stay schemars-simple); timestamps are RFC3339 strings.

use serde::Deserialize;
use serde_json::Value;

/// `crucible_trace_open_span` + `crucible_trace_record_span` (record_span is the
/// one-shot open+close the `crucible-trace` extension uses per dispatched step).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CrucibleTraceSpanParams {
    #[schemars(description = "Run correlation id (UUID) minted at orchestration start.")]
    pub trace_id: String,
    #[schemars(
        description = "Span kind: run|plan|synthesize|fv_gate|planned_step|call_frame|critic_iteration|tool_call|red_team|validate."
    )]
    pub kind: String,
    #[schemars(description = "Human label, e.g. \"PlannedStep:2 -> W1 (t1_req)\".")]
    pub name: String,
    #[serde(default)]
    #[schemars(description = "Parent span id (for the span tree); omit for the run root.")]
    pub parent_span_id: Option<i64>,
    #[serde(default)]
    #[schemars(description = "record_span only: terminal status ok|error|canceled (default ok).")]
    pub status: Option<String>,
    #[serde(default)]
    #[schemars(description = "Error/cancel detail.")]
    pub status_message: Option<String>,
    #[serde(default)]
    #[schemars(description = "Orchestration session_key this span belongs to.")]
    pub session_key: Option<String>,
    #[serde(default)]
    #[schemars(description = "Per-step a2a_task id (UUID).")]
    pub task_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "csm_run_traces.id the span's events live in.")]
    pub run_trace_id: Option<i64>,
    #[serde(default)]
    #[schemars(description = "work_item public_id.")]
    pub work_item_public_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "Linked experiment id.")]
    pub experiment_id: Option<i64>,
    #[serde(default)]
    #[schemars(description = "pi conversation correlation id.")]
    pub pi_session_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "Acting protocol role (O|C|W0|...).")]
    pub role: Option<String>,
    #[serde(default)]
    #[schemars(description = "Bound fleet peer.")]
    pub peer: Option<String>,
    #[serde(default)]
    #[schemars(description = "Model id chosen for this step (from models.json).")]
    pub model: Option<String>,
    #[serde(default)]
    #[schemars(description = "Inclusive start index into csm_run_traces.events.")]
    pub event_lo: Option<i32>,
    #[serde(default)]
    #[schemars(description = "Exclusive end index into csm_run_traces.events.")]
    pub event_hi: Option<i32>,
    #[serde(default)]
    #[schemars(description = "The orchestrator cursor (PlannedStep count) at span entry.")]
    pub gtype_cursor: Option<i32>,
    #[serde(default)]
    #[schemars(description = "Pushdown stack depth at span entry (default 0).")]
    pub frame_depth: Option<i32>,
    #[serde(default)]
    #[schemars(description = "The orchestrator LocalState at span entry (validated on replay).")]
    pub orch_state: Option<i32>,
    #[serde(default)]
    #[schemars(description = "Critic-loop iteration (the non-FSM-recoverable counter).")]
    pub critic_iteration: Option<i32>,
    #[serde(default)]
    #[schemars(description = "Critic phase: attempt|awaiting_verdict|pass|revise.")]
    pub critic_phase: Option<String>,
    #[serde(default)]
    #[schemars(description = "Arbitrary structured attributes (tokens, file:line, etc.).")]
    pub attributes: Option<Value>,
    #[serde(default)]
    #[schemars(description = "Cross-trace links [{trace_id, span_id?, rel}].")]
    pub links: Option<Value>,
}

/// `crucible_trace_close_span`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CrucibleTraceCloseParams {
    #[schemars(description = "The span to close.")]
    pub span_id: i64,
    #[serde(default)]
    #[schemars(description = "Terminal status ok|error|canceled (default ok).")]
    pub status: Option<String>,
    #[serde(default)]
    #[schemars(description = "Error/cancel detail.")]
    pub status_message: Option<String>,
}

/// `crucible_trace_event` — append a point-in-time annotation to a span.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CrucibleTraceEventParams {
    #[schemars(description = "Owning span id.")]
    pub span_id: i64,
    #[schemars(description = "Run correlation id (UUID).")]
    pub trace_id: String,
    #[schemars(
        description = "Annotation kind: model_chosen|retry|failure|counterexample_found|fv_verdict|critic_verdict|halt|resume|cancel|lease_lost|conformance_fail|off_protocol."
    )]
    pub event_kind: String,
    #[serde(default)]
    #[schemars(description = "Severity info|warn|error (default info).")]
    pub severity: Option<String>,
    #[serde(default)]
    #[schemars(description = "Human message.")]
    pub message: Option<String>,
    #[serde(default)]
    #[schemars(description = "Index into csm_run_traces.events this pins to.")]
    pub event_ord: Option<i32>,
    #[serde(default)]
    #[schemars(description = "Linked counterexample id (for counterexample_found).")]
    pub counterexample_id: Option<i64>,
    #[serde(default)]
    #[schemars(description = "Structured payload.")]
    pub attributes: Option<Value>,
}

/// `crucible_trace_record_counterexample` — persist a replayable witness (ADR-020/D3).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CrucibleRecordCexParams {
    #[schemars(
        description = "Checker source: tlc|smt|rocq|sfa|presburger|kat|conformance|behavioral."
    )]
    pub source: String,
    #[schemars(
        description = "Witness shape: event_trace|state_assignment|smt_model|tla_trace|proof_term."
    )]
    pub witness_kind: String,
    #[schemars(description = "The structured, machine-replayable witness (JSON).")]
    pub witness: Value,
    #[schemars(description = "SHA-256 of the raw checker output (idempotency key).")]
    pub content_sha256: String,
    #[serde(default)]
    #[schemars(description = "Run correlation id (UUID).")]
    pub trace_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "Owning span id.")]
    pub span_id: Option<i64>,
    #[serde(default)]
    #[schemars(description = "Linked experiment id.")]
    pub experiment_id: Option<i64>,
    #[serde(default)]
    #[schemars(description = "work_item public_id (the obligation).")]
    pub work_item_public_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "Verdict violated|sat|unsat_core|timeout (default violated).")]
    pub verdict: Option<String>,
    #[serde(default)]
    #[schemars(description = "The named invariant/obligation that failed.")]
    pub property: Option<String>,
    #[serde(default)]
    #[schemars(description = "Raw checker stdout (human-readable mirror).")]
    pub content: Option<String>,
    #[serde(default)]
    #[schemars(description = "Metrics {depth, states, time_ms}.")]
    pub metrics: Option<Value>,
}

/// `crucible_trace_control` — append a control-plane action to the audit journal.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CrucibleControlParams {
    #[schemars(
        description = "Action: halt|resume|checkpoint|cancel|fork|lease_expire|power_fail."
    )]
    pub action: String,
    #[serde(default)]
    #[schemars(description = "Scope: fleet|session|task|work_item (default fleet).")]
    pub scope: Option<String>,
    #[serde(default)]
    #[schemars(description = "Affected session_key.")]
    pub session_key: Option<String>,
    #[serde(default)]
    #[schemars(description = "Affected a2a_task id (UUID).")]
    pub task_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "Affected work_item public_id.")]
    pub work_item_public_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "Correlated run trace id (UUID).")]
    pub trace_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "Correlated span id.")]
    pub span_id: Option<i64>,
    #[serde(default)]
    #[schemars(description = "Why (the {reason} plumbed through the control plane).")]
    pub reason: Option<String>,
    #[serde(default)]
    #[schemars(description = "Channel/actor: cli|rest|mcp|tripfile|ups|cron.")]
    pub actor: Option<String>,
    #[serde(default)]
    #[schemars(description = "Structured payload.")]
    pub attributes: Option<Value>,
}

/// `crucible_trace_get` / `_reconcile` / `_why` — reference one run.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CrucibleTraceRefParams {
    #[serde(default)]
    #[schemars(description = "Run correlation id (UUID).")]
    pub trace_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "Orchestration session_key (alternative to trace_id).")]
    pub session_key: Option<String>,
}

/// `crucible_trace_timeline`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CrucibleTraceTimelineParams {
    #[serde(default)]
    #[schemars(description = "Run correlation id (UUID).")]
    pub trace_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "Orchestration session_key (alternative to trace_id).")]
    pub session_key: Option<String>,
    #[serde(default)]
    #[schemars(description = "Include point-in-time annotations (default true).")]
    pub include_annotations: Option<bool>,
}

/// `crucible_trace_query` — cross-trace span filter.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CrucibleTraceQueryParams {
    #[serde(default)]
    #[schemars(description = "Filter by span kind.")]
    pub kind: Option<String>,
    #[serde(default)]
    #[schemars(description = "Filter by status (unset|ok|error|canceled).")]
    pub status: Option<String>,
    #[serde(default)]
    #[schemars(description = "Filter by acting role.")]
    pub role: Option<String>,
    #[serde(default)]
    #[schemars(description = "Filter by model id.")]
    pub model: Option<String>,
    #[serde(default)]
    #[schemars(description = "Filter by work_item public_id.")]
    pub work_item_public_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "Filter by experiment id.")]
    pub experiment_id: Option<i64>,
    #[serde(default)]
    #[schemars(description = "Started at or after (RFC3339).")]
    pub since: Option<String>,
    #[serde(default)]
    #[schemars(description = "Started at or before (RFC3339).")]
    pub until: Option<String>,
    #[serde(default)]
    #[schemars(description = "Max rows (default 100, clamped 1..=10000).")]
    pub limit: Option<i64>,
}

/// `crucible_trace_replay` — step-debug a recorded run.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CrucibleTraceReplayParams {
    #[serde(default)]
    #[schemars(description = "Run correlation id (UUID).")]
    pub trace_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "Orchestration session_key (alternative to trace_id).")]
    pub session_key: Option<String>,
    #[serde(default)]
    #[schemars(description = "Replay this many recorded events (default: the whole trace).")]
    pub to_event: Option<i64>,
    #[serde(default)]
    #[schemars(description = "Replay this many PlannedSteps (req/resp pairs) = 2*to_step events.")]
    pub to_step: Option<i64>,
}

/// `crucible_trace_diff` — structural diff of a failing vs passing run.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CrucibleTraceDiffParams {
    #[schemars(description = "The failing run's trace_id (UUID).")]
    pub failing: String,
    #[schemars(description = "The passing/reference run's trace_id (UUID).")]
    pub passing: String,
}

/// `crucible_trace_audit` — the control-plane journal.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CrucibleTraceAuditParams {
    #[serde(default)]
    #[schemars(description = "Filter by action.")]
    pub action: Option<String>,
    #[serde(default)]
    #[schemars(description = "Filter by scope.")]
    pub scope: Option<String>,
    #[serde(default)]
    #[schemars(description = "Filter by session_key.")]
    pub session_key: Option<String>,
    #[serde(default)]
    #[schemars(description = "At or after (RFC3339).")]
    pub since: Option<String>,
    #[serde(default)]
    #[schemars(description = "Max rows (default 100, clamped 1..=10000).")]
    pub limit: Option<i64>,
}

/// `crucible_trace_counterexample` — fetch a persisted witness.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CrucibleTraceCexParams {
    #[serde(default)]
    #[schemars(description = "Fetch by counterexample id.")]
    pub id: Option<i64>,
    #[serde(default)]
    #[schemars(description = "Or the latest for this run trace_id (UUID).")]
    pub trace_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "Optionally restrict to a source (tlc|smt|rocq|...).")]
    pub source: Option<String>,
}
