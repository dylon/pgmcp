//! `crucible_trace_*` — unified run tracing record/query/replay (ADR-020 E10).
//!
//! ## Boundary
//!
//! Record tools write pgmcp's OWN trace tables (memory class); query/replay tools are
//! pure SELECTs + the in-memory [`crate::csm::trace_store`] replay (side-effect-free).
//! No file write, no checker, no shell — analytical + memory only (architecture §4).
//! Replay reuses `replay_to_configs`/`next_step_from` UNCHANGED (ADR-011 soundness):
//! a corrupt prefix makes replay refuse loudly, never reconstruct wrong.

use std::sync::atomic::Ordering;

use chrono::{DateTime, Utc};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::context::SystemContext;
use crate::csm::conformance::Event;
use crate::csm::machine::Network;
use crate::csm::mpst::global::GlobalType;
use crate::csm::role::Role;
use crate::csm::session_store::load_checkpoint;
use crate::csm::trace_store::{
    self as ts, AnnotationInput, AnnotationKind, AnnotationSeverity, CexSource, CexVerdict,
    ControlAction, ControlInput, ControlScope, CounterexampleInput, SpanInput, SpanKind, SpanStatus,
    WitnessKind,
};
use crate::mcp::server::{
    CrucibleControlParams, CrucibleRecordCexParams, CrucibleTraceAuditParams, CrucibleTraceCexParams,
    CrucibleTraceCloseParams, CrucibleTraceDiffParams, CrucibleTraceEventParams,
    CrucibleTraceQueryParams, CrucibleTraceRefParams, CrucibleTraceReplayParams,
    CrucibleTraceSpanParams, CrucibleTraceTimelineParams,
};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

// ── small shared helpers ────────────────────────────────────────────────────

fn internal(e: impl std::fmt::Display) -> McpError {
    McpError::internal_error(e.to_string(), None)
}
fn parse_uuid(s: &str) -> Result<Uuid, McpError> {
    Uuid::parse_str(s).map_err(|e| McpError::invalid_params(format!("invalid UUID '{s}': {e}"), None))
}
fn parse_opt_uuid(s: &Option<String>) -> Result<Option<Uuid>, McpError> {
    s.as_deref().map(parse_uuid).transpose()
}
fn parse_ts(s: &str) -> Result<DateTime<Utc>, McpError> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| McpError::invalid_params(format!("invalid RFC3339 '{s}': {e}"), None))
}
fn parse_opt_ts(s: &Option<String>) -> Result<Option<DateTime<Utc>>, McpError> {
    s.as_deref().map(parse_ts).transpose()
}
fn req<T>(v: Option<T>, what: &str) -> Result<T, McpError> {
    v.ok_or_else(|| McpError::invalid_params(format!("invalid {what}"), None))
}
fn to_value<T: serde::Serialize>(v: &T) -> Result<Value, McpError> {
    serde_json::to_value(v).map_err(internal)
}
fn empty_obj() -> Value {
    Value::Object(Default::default())
}
fn empty_arr() -> Value {
    Value::Array(Default::default())
}

/// Build a [`SpanInput`] from the shared span params with an explicit status/end.
fn build_span_input(
    p: &CrucibleTraceSpanParams,
    status: SpanStatus,
    ended_at: Option<DateTime<Utc>>,
) -> Result<SpanInput, McpError> {
    Ok(SpanInput {
        trace_id: parse_uuid(&p.trace_id)?,
        parent_span_id: p.parent_span_id,
        kind: req(SpanKind::parse(&p.kind), &format!("span kind '{}'", p.kind))?,
        name: p.name.clone(),
        status,
        status_message: p.status_message.clone(),
        ended_at,
        session_key: p.session_key.clone(),
        task_id: parse_opt_uuid(&p.task_id)?,
        run_trace_id: p.run_trace_id,
        work_item_public_id: p.work_item_public_id.clone(),
        experiment_id: p.experiment_id,
        pi_session_id: p.pi_session_id.clone(),
        role: p.role.clone(),
        peer: p.peer.clone(),
        model: p.model.clone(),
        event_lo: p.event_lo,
        event_hi: p.event_hi,
        gtype_cursor: p.gtype_cursor,
        frame_depth: p.frame_depth.unwrap_or(0),
        orch_state: p.orch_state,
        critic_iteration: p.critic_iteration,
        critic_phase: p.critic_phase.clone(),
        attributes: p.attributes.clone().unwrap_or_else(empty_obj),
        links: p.links.clone().unwrap_or_else(empty_arr),
    })
}

/// Resolve a run to `(Network, events, orchestrator role)` from a trace_id or
/// session_key — the shared setup for replay/reconcile/why. The planned `Network` is
/// rebuilt from the session's stored `global_type`; the events are the durable
/// `csm_run_traces` trace (falling back to the checkpoint's unflushed transcript).
async fn resolve_run(
    pool: &PgPool,
    trace_id: &Option<String>,
    session_key: &Option<String>,
) -> Result<(Network, Vec<Event>, Role), McpError> {
    let sk: String = if let Some(s) = session_key {
        s.clone()
    } else {
        let tid = parse_uuid(req(trace_id.as_deref(), "trace_id or session_key")?)?;
        ts::session_key_for_trace(pool, tid)
            .await
            .map_err(internal)?
            .ok_or_else(|| {
                McpError::invalid_params(
                    format!("no session linked to trace {tid}; replay needs the run's global_type"),
                    None,
                )
            })?
    };
    let cp = load_checkpoint(pool, &sk)
        .await
        .map_err(internal)?
        .ok_or_else(|| McpError::invalid_params(format!("no orchestration session '{sk}'"), None))?;
    let g: GlobalType = serde_json::from_value(cp.global_type.clone())
        .map_err(|e| internal(format!("stored global_type is not a GlobalType: {e}")))?;
    let net = Network::build(cp.protocol_name.clone(), &g)
        .map_err(|e| internal(format!("network build failed: {e:?}")))?;
    let events = match cp.task_id {
        Some(tid) => {
            let ev = ts::load_run_events(pool, tid).await.map_err(internal)?;
            if ev.is_empty() {
                cp.transcript_events()
            } else {
                ev
            }
        }
        None => cp.transcript_events(),
    };
    Ok((net, events, Role::new(cp.orchestrator_role.clone())))
}

// ── record (write) tools ────────────────────────────────────────────────────

/// Open a span (status `unset`, no `ended_at`) — for long spans whose partial record
/// on crash is valuable (e.g. an FV discharge running `tlc`).
pub async fn tool_crucible_trace_open_span(
    ctx: &SystemContext,
    params: CrucibleTraceSpanParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let input = build_span_input(&params, SpanStatus::Unset, None)?;
    let span_id = ts::record_span(pool, &input).await.map_err(internal)?;
    json_result(&json!({ "span_id": span_id, "trace_id": params.trace_id, "open": true }))
}

/// Record a complete span in one shot (open + close) — the primary path the
/// `crucible-trace` extension uses per dispatched step.
pub async fn tool_crucible_trace_record_span(
    ctx: &SystemContext,
    params: CrucibleTraceSpanParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let status = match params.status.as_deref() {
        None => SpanStatus::Ok,
        Some(s) => req(SpanStatus::parse(s), &format!("status '{s}'"))?,
    };
    let input = build_span_input(&params, status, Some(Utc::now()))?;
    let span_id = ts::record_span(pool, &input).await.map_err(internal)?;
    json_result(&json!({ "span_id": span_id, "trace_id": params.trace_id, "status": status.as_str() }))
}

/// Close an open span: set its terminal status + `ended_at`.
pub async fn tool_crucible_trace_close_span(
    ctx: &SystemContext,
    params: CrucibleTraceCloseParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let status = match params.status.as_deref() {
        None => SpanStatus::Ok,
        Some(s) => req(SpanStatus::parse(s), &format!("status '{s}'"))?,
    };
    let row = ts::close_span(pool, params.span_id, status, params.status_message.as_deref(), None)
        .await
        .map_err(internal)?
        .ok_or_else(|| McpError::invalid_params(format!("no span {}", params.span_id), None))?;
    json_result(&to_value(&row)?)
}

/// Append a point-in-time annotation to a span.
pub async fn tool_crucible_trace_event(
    ctx: &SystemContext,
    params: CrucibleTraceEventParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let input = AnnotationInput {
        span_id: params.span_id,
        trace_id: parse_uuid(&params.trace_id)?,
        event_kind: req(
            AnnotationKind::parse(&params.event_kind),
            &format!("event_kind '{}'", params.event_kind),
        )?,
        severity: match params.severity.as_deref() {
            None => AnnotationSeverity::Info,
            Some(s) => req(AnnotationSeverity::parse(s), &format!("severity '{s}'"))?,
        },
        message: params.message.clone(),
        event_ord: params.event_ord,
        counterexample_id: params.counterexample_id,
        attributes: params.attributes.clone().unwrap_or_else(empty_obj),
    };
    let id = ts::record_annotation(pool, &input).await.map_err(internal)?;
    json_result(&json!({ "id": id }))
}

/// Persist a counterexample witness (idempotent on `content_sha256`).
pub async fn tool_crucible_trace_record_counterexample(
    ctx: &SystemContext,
    params: CrucibleRecordCexParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let input = CounterexampleInput {
        trace_id: parse_opt_uuid(&params.trace_id)?,
        span_id: params.span_id,
        experiment_id: params.experiment_id,
        work_item_public_id: params.work_item_public_id.clone(),
        source: req(CexSource::parse(&params.source), &format!("source '{}'", params.source))?,
        verdict: match params.verdict.as_deref() {
            None => CexVerdict::Violated,
            Some(s) => req(CexVerdict::parse(s), &format!("verdict '{s}'"))?,
        },
        property: params.property.clone(),
        witness_kind: req(
            WitnessKind::parse(&params.witness_kind),
            &format!("witness_kind '{}'", params.witness_kind),
        )?,
        witness: params.witness.clone(),
        content: params.content.clone(),
        content_sha256: params.content_sha256.clone(),
        metrics: params.metrics.clone().unwrap_or_else(empty_obj),
    };
    let id = ts::record_counterexample(pool, &input).await.map_err(internal)?;
    json_result(&json!({ "id": id }))
}

/// Append a control-plane action to the append-only audit journal (ADR-016/D4).
pub async fn tool_crucible_trace_control(
    ctx: &SystemContext,
    params: CrucibleControlParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let input = ControlInput {
        action: req(ControlAction::parse(&params.action), &format!("action '{}'", params.action))?,
        scope: match params.scope.as_deref() {
            None => ControlScope::Fleet,
            Some(s) => req(ControlScope::parse(s), &format!("scope '{s}'"))?,
        },
        session_key: params.session_key.clone(),
        task_id: parse_opt_uuid(&params.task_id)?,
        work_item_public_id: params.work_item_public_id.clone(),
        trace_id: parse_opt_uuid(&params.trace_id)?,
        span_id: params.span_id,
        reason: params.reason.clone(),
        actor: params.actor.clone(),
        attributes: params.attributes.clone().unwrap_or_else(empty_obj),
    };
    let id = ts::record_control(pool, &input).await.map_err(internal)?;
    json_result(&json!({ "id": id }))
}

// ── query / replay (read) tools ─────────────────────────────────────────────

/// Run header + summary counts.
pub async fn tool_crucible_trace_get(
    ctx: &SystemContext,
    params: CrucibleTraceRefParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let tid = resolve_trace_id(pool, &params.trace_id, &params.session_key).await?;
    let header = ts::trace_header(pool, tid)
        .await
        .map_err(internal)?
        .ok_or_else(|| McpError::invalid_params(format!("no spans for trace {tid}"), None))?;
    json_result(&to_value(&header)?)
}

/// The ordered span timeline (+ annotations).
pub async fn tool_crucible_trace_timeline(
    ctx: &SystemContext,
    params: CrucibleTraceTimelineParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let spans = match (&params.trace_id, &params.session_key) {
        (Some(t), _) => ts::load_spans(pool, parse_uuid(t)?).await,
        (None, Some(s)) => ts::load_spans_by_session(pool, s).await,
        (None, None) => {
            return Err(McpError::invalid_params("provide trace_id or session_key", None));
        }
    }
    .map_err(internal)?;
    let mut out = json!({ "count": spans.len(), "spans": to_value(&spans)? });
    if params.include_annotations.unwrap_or(true) {
        if let Some(first) = spans.first() {
            let anns = ts::load_annotations(pool, first.trace_id).await.map_err(internal)?;
            out["annotations"] = to_value(&anns)?;
        }
    }
    json_result(&out)
}

/// Cross-trace span filter.
pub async fn tool_crucible_trace_query(
    ctx: &SystemContext,
    params: CrucibleTraceQueryParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let q = ts::SpanQuery {
        kind: params.kind.clone(),
        status: params.status.clone(),
        role: params.role.clone(),
        model: params.model.clone(),
        work_item_public_id: params.work_item_public_id.clone(),
        experiment_id: params.experiment_id,
        since: parse_opt_ts(&params.since)?,
        until: parse_opt_ts(&params.until)?,
        limit: params.limit.unwrap_or(100),
    };
    let spans = ts::query_spans(pool, &q).await.map_err(internal)?;
    json_result(&json!({ "count": spans.len(), "spans": to_value(&spans)? }))
}

/// Step-debug a recorded run: replay a prefix and recover the position + next move.
pub async fn tool_crucible_trace_replay(
    ctx: &SystemContext,
    params: CrucibleTraceReplayParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let (net, events, orch) = resolve_run(pool, &params.trace_id, &params.session_key).await?;
    let to_event = params
        .to_event
        .or_else(|| params.to_step.map(|s| s * 2))
        .map(|n| n.clamp(0, events.len() as i64) as usize)
        .unwrap_or(events.len());
    match ts::replay_position(&net, &events[..to_event], &orch) {
        Ok(pos) => json_result(&json!({
            "to_event": to_event,
            "conformant_prefix": true,
            "position": to_value(&pos)?,
        })),
        Err(div) => json_result(&json!({
            "to_event": to_event,
            "conformant_prefix": false,
            "refused": "replay landed on an off-protocol event; refusing loudly (ADR-011 anti-desync)",
            "divergence": to_value(&div)?,
        })),
    }
}

/// Reconcile the observed run against the planned protocol → the first divergence.
pub async fn tool_crucible_trace_reconcile(
    ctx: &SystemContext,
    params: CrucibleTraceRefParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let (net, events, orch) = resolve_run(pool, &params.trace_id, &params.session_key).await?;
    let div = ts::first_divergence(&net, &events, &orch);
    json_result(&json!({
        "events": events.len(),
        "conformant": div.is_none(),
        "first_divergence": div.map(|d| to_value(&d)).transpose()?,
    }))
}

/// Walk to the first failure + its cause: the first `error` span, the protocol-level
/// first divergence, and the replayed position there.
pub async fn tool_crucible_trace_why(
    ctx: &SystemContext,
    params: CrucibleTraceRefParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let spans = match (&params.trace_id, &params.session_key) {
        (Some(t), _) => ts::load_spans(pool, parse_uuid(t)?).await,
        (None, Some(s)) => ts::load_spans_by_session(pool, s).await,
        (None, None) => {
            return Err(McpError::invalid_params("provide trace_id or session_key", None));
        }
    }
    .map_err(internal)?;
    let first_error = spans.iter().find(|s| s.status == "error");

    // Protocol-level reconcile (best-effort: only if the run has a linked session).
    let (divergence, position) = match resolve_run(pool, &params.trace_id, &params.session_key).await
    {
        Ok((net, events, orch)) => {
            let div = ts::first_divergence(&net, &events, &orch);
            let pos = match &div {
                Some(ts::Divergence::OffProtocol { step, .. }) => {
                    ts::replay_position(&net, &events[..*step], &orch).ok()
                }
                Some(ts::Divergence::Stall { .. }) | Some(ts::Divergence::Unbalanced { .. }) => {
                    ts::replay_position(&net, &events, &orch).ok()
                }
                None => None,
            };
            (div.map(|d| to_value(&d)).transpose()?, pos.map(|p| to_value(&p)).transpose()?)
        }
        Err(_) => (None, None),
    };

    json_result(&json!({
        "first_error_span": first_error.map(to_value).transpose()?,
        "first_divergence": divergence,
        "position": position,
        "advice": "patch the plan (add depends_on / fix the role binding / fix the Critic gate) then re-verify by replay (ADR-020).",
    }))
}

/// Structural diff of a failing vs passing run, aligned on `(gtype_cursor, kind, role)`.
pub async fn tool_crucible_trace_diff(
    ctx: &SystemContext,
    params: CrucibleTraceDiffParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let fail = ts::load_spans(pool, parse_uuid(&params.failing)?).await.map_err(internal)?;
    let pass = ts::load_spans(pool, parse_uuid(&params.passing)?).await.map_err(internal)?;

    // Align step spans by cursor; report the first cursor whose (kind, role, status,
    // peer, model) differs — the regression "what changed".
    let key = |s: &ts::TraceSpan| (s.gtype_cursor, s.kind.clone());
    let mut first_divergence: Option<Value> = None;
    let mut f_iter = fail.iter().filter(|s| s.gtype_cursor.is_some());
    let mut p_iter = pass.iter().filter(|s| s.gtype_cursor.is_some());
    loop {
        match (f_iter.next(), p_iter.next()) {
            (Some(f), Some(p)) => {
                if key(f) != key(p)
                    || f.role != p.role
                    || f.status != p.status
                    || f.peer != p.peer
                    || f.model != p.model
                {
                    first_divergence = Some(json!({
                        "cursor": f.gtype_cursor,
                        "failing": { "kind": f.kind, "role": f.role, "status": f.status, "peer": f.peer, "model": f.model },
                        "passing": { "kind": p.kind, "role": p.role, "status": p.status, "peer": p.peer, "model": p.model },
                    }));
                    break;
                }
            }
            (Some(f), None) => {
                first_divergence = Some(json!({ "cursor": f.gtype_cursor, "failing_only": f.kind }));
                break;
            }
            (None, Some(p)) => {
                first_divergence = Some(json!({ "cursor": p.gtype_cursor, "passing_only": p.kind }));
                break;
            }
            (None, None) => break,
        }
    }
    json_result(&json!({
        "failing_spans": fail.len(),
        "passing_spans": pass.len(),
        "first_divergence": first_divergence,
        "identical": first_divergence.is_none(),
    }))
}

/// The control-plane audit history.
pub async fn tool_crucible_trace_audit(
    ctx: &SystemContext,
    params: CrucibleTraceAuditParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let q = ts::ControlQuery {
        action: params.action.clone(),
        scope: params.scope.clone(),
        session_key: params.session_key.clone(),
        since: parse_opt_ts(&params.since)?,
        limit: params.limit.unwrap_or(100),
    };
    let rows = ts::load_control_journal(pool, &q).await.map_err(internal)?;
    json_result(&json!({ "count": rows.len(), "events": to_value(&rows)? }))
}

/// Fetch a persisted counterexample witness.
pub async fn tool_crucible_trace_counterexample(
    ctx: &SystemContext,
    params: CrucibleTraceCexParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let tid = parse_opt_uuid(&params.trace_id)?;
    let cex = ts::load_counterexample(pool, params.id, tid, params.source.as_deref())
        .await
        .map_err(internal)?
        .ok_or_else(|| McpError::invalid_params("no matching counterexample", None))?;
    json_result(&to_value(&cex)?)
}

/// Resolve a trace_id from explicit trace_id or a session_key's first span.
async fn resolve_trace_id(
    pool: &PgPool,
    trace_id: &Option<String>,
    session_key: &Option<String>,
) -> Result<Uuid, McpError> {
    if let Some(t) = trace_id {
        return parse_uuid(t);
    }
    let sk = req(session_key.as_deref(), "trace_id or session_key")?;
    let spans = ts::load_spans_by_session(pool, sk).await.map_err(internal)?;
    spans
        .first()
        .map(|s| s.trace_id)
        .ok_or_else(|| McpError::invalid_params(format!("no spans for session '{sk}'"), None))
}
