//! DB read/write + pure replay/reconcile for **unified run tracing** (ADR-020, E10).
//!
//! Mirrors [`crate::csm::session_store`]: pure persistence over pgmcp's OWN trace
//! tables (migration `v60_crucible_trace`) plus the *pure* replay/reconcile logic
//! the `crucible_trace_*` tools call. No file I/O, no shell, no checker — analytical
//! + memory only (the no-file boundary, architecture §4).
//!
//! ## The model
//!
//! A **span** ([`SpanInput`] / [`TraceSpan`]) is an OTel-shaped node annotating a
//! slice `[event_lo, event_hi)` of `csm_run_traces.events`; it binds to the machine
//! via `(orch_state, frame_depth, gtype_cursor, critic_iteration)`. Spans carry NO
//! protocol semantics not derivable from their event slice — they *annotate* the
//! event trace, which stays the sole source of position (ADR-011). Replay therefore
//! reuses [`crate::csm::conformance::replay_to_configs`] /
//! [`crate::csm::driver::ProtocolDriver::next_step_from`] UNCHANGED (no new
//! soundness surface): a span's cached `orch_state` is *validated against* the
//! replay result, never trusted over it.
//!
//! [`first_divergence`] is the heart of `crucible_trace_reconcile`/`_why`: replay an
//! observed trace against the planned `Network` and return the first point it left
//! the plan — off-protocol (an illegal event) or a stall (a clean prefix that never
//! reaches terminal). It is pure (no DB), so it is fully unit-tested below.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgPool, QueryBuilder};
use uuid::Uuid;

use crate::csm::conformance::{ConformanceError, Event, replay_to_configs};
use crate::csm::driver::ProtocolDriver;
use crate::csm::machine::Network;
use crate::csm::role::Role;
use crate::tracker::kind::join_quoted;

// ============================================================================
// Closed vocabularies (ADR-003 idiom: TEXT + CHECK built from the Rust enum's
// `sql_in_list`, a golden test pins each — the single source of truth shared with
// the v60 table CHECK constraints, so the DB and Rust cannot drift).
// ============================================================================

macro_rules! closed_vocab {
    ($(#[$m:meta])* $name:ident { $( $variant:ident => $s:literal ),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $( $variant ),+ }
        impl $name {
            /// Canonical ordering; also the source of the DB CHECK vocabulary.
            pub const ALL: &'static [$name] = &[ $( Self::$variant ),+ ];
            pub fn as_str(self) -> &'static str { match self { $( Self::$variant => $s ),+ } }
            pub fn parse(s: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|k| k.as_str() == s)
            }
            /// SQL `IN (...)` value list built from [`Self::ALL`].
            pub fn sql_in_list() -> String { join_quoted(Self::ALL.iter().map(|k| k.as_str())) }
        }
    };
}

closed_vocab!(
    /// The kind of a trace span — the run-shape vocabulary (ADR-020/D1).
    SpanKind {
        Run => "run", Plan => "plan", Synthesize => "synthesize", FvGate => "fv_gate",
        PlannedStep => "planned_step", CallFrame => "call_frame",
        CriticIteration => "critic_iteration", ToolCall => "tool_call",
        RedTeam => "red_team", Validate => "validate",
    }
);
closed_vocab!(
    /// Terminal status of a span (OTel-style).
    SpanStatus { Unset => "unset", Ok => "ok", Error => "error", Canceled => "canceled" }
);
closed_vocab!(
    /// A point-in-time annotation within a span (distinct from a protocol `Event`).
    AnnotationKind {
        ModelChosen => "model_chosen", Retry => "retry", Failure => "failure",
        CounterexampleFound => "counterexample_found", FvVerdict => "fv_verdict",
        CriticVerdict => "critic_verdict", Halt => "halt", Resume => "resume",
        Cancel => "cancel", LeaseLost => "lease_lost", ConformanceFail => "conformance_fail",
        OffProtocol => "off_protocol",
    }
);
closed_vocab!(
    /// Severity of an annotation.
    AnnotationSeverity { Info => "info", Warn => "warn", Error => "error" }
);
closed_vocab!(
    /// A control-plane action recorded in the append-only journal (ADR-016/ADR-020/D4).
    ControlAction {
        Halt => "halt", Resume => "resume", Checkpoint => "checkpoint", Cancel => "cancel",
        Fork => "fork", LeaseExpire => "lease_expire", PowerFail => "power_fail",
    }
);
closed_vocab!(
    /// Scope of a control action.
    ControlScope { Fleet => "fleet", Session => "session", Task => "task", WorkItem => "work_item" }
);
closed_vocab!(
    /// The checker that produced a counterexample witness (ADR-020/D3).
    CexSource {
        Tlc => "tlc", Smt => "smt", Rocq => "rocq", Sfa => "sfa", Presburger => "presburger",
        Kat => "kat", Conformance => "conformance", Behavioral => "behavioral",
    }
);
closed_vocab!(
    /// The verdict shape of a counterexample.
    CexVerdict { Violated => "violated", Sat => "sat", UnsatCore => "unsat_core", Timeout => "timeout" }
);
closed_vocab!(
    /// The structured shape of a replayable witness.
    WitnessKind {
        EventTrace => "event_trace", StateAssignment => "state_assignment",
        SmtModel => "smt_model", TlaTrace => "tla_trace", ProofTerm => "proof_term",
    }
);

// ============================================================================
// Row structs (FromRow) + input structs.
// ============================================================================

/// One `crucible_trace_spans` row.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct TraceSpan {
    pub span_id: i64,
    pub trace_id: Uuid,
    pub parent_span_id: Option<i64>,
    pub kind: String,
    pub name: String,
    pub status: String,
    pub status_message: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub session_key: Option<String>,
    pub task_id: Option<Uuid>,
    pub run_trace_id: Option<i64>,
    pub work_item_public_id: Option<String>,
    pub experiment_id: Option<i64>,
    pub pi_session_id: Option<String>,
    pub role: Option<String>,
    pub peer: Option<String>,
    pub model: Option<String>,
    pub event_lo: Option<i32>,
    pub event_hi: Option<i32>,
    pub gtype_cursor: Option<i32>,
    pub frame_depth: i32,
    pub orch_state: Option<i32>,
    pub critic_iteration: Option<i32>,
    pub critic_phase: Option<String>,
    pub attributes: Value,
    pub links: Value,
    pub created_at: DateTime<Utc>,
}

/// The agent-provided values for one span (the orchestrator/extension supplies all
/// of them; pgmcp persists verbatim). Constructed fully at every call site — no
/// `Default` (a defaulted `attributes`/`links` would be `Null`, violating the table's
/// `NOT NULL` columns; the tool layer always fills them with `{}` / `[]`).
#[derive(Debug, Clone)]
pub struct SpanInput {
    pub trace_id: Uuid,
    pub parent_span_id: Option<i64>,
    pub kind: SpanKind,
    pub name: String,
    pub status: SpanStatus,
    pub status_message: Option<String>,
    pub ended_at: Option<DateTime<Utc>>,
    pub session_key: Option<String>,
    pub task_id: Option<Uuid>,
    pub run_trace_id: Option<i64>,
    pub work_item_public_id: Option<String>,
    pub experiment_id: Option<i64>,
    pub pi_session_id: Option<String>,
    pub role: Option<String>,
    pub peer: Option<String>,
    pub model: Option<String>,
    pub event_lo: Option<i32>,
    pub event_hi: Option<i32>,
    pub gtype_cursor: Option<i32>,
    pub frame_depth: i32,
    pub orch_state: Option<i32>,
    pub critic_iteration: Option<i32>,
    pub critic_phase: Option<String>,
    pub attributes: Value,
    pub links: Value,
}

/// One `crucible_trace_events` (annotation) row.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct TraceAnnotation {
    pub id: i64,
    pub span_id: i64,
    pub trace_id: Uuid,
    pub at: DateTime<Utc>,
    pub event_kind: String,
    pub severity: String,
    pub message: Option<String>,
    pub event_ord: Option<i32>,
    pub counterexample_id: Option<i64>,
    pub attributes: Value,
}

/// Input for one annotation.
#[derive(Debug, Clone)]
pub struct AnnotationInput {
    pub span_id: i64,
    pub trace_id: Uuid,
    pub event_kind: AnnotationKind,
    pub severity: AnnotationSeverity,
    pub message: Option<String>,
    pub event_ord: Option<i32>,
    pub counterexample_id: Option<i64>,
    pub attributes: Value,
}

/// One `crucible_control_journal` row.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct ControlJournalEntry {
    pub id: i64,
    pub action: String,
    pub scope: String,
    pub session_key: Option<String>,
    pub task_id: Option<Uuid>,
    pub work_item_public_id: Option<String>,
    pub trace_id: Option<Uuid>,
    pub span_id: Option<i64>,
    pub reason: Option<String>,
    pub actor: Option<String>,
    pub at: DateTime<Utc>,
    pub attributes: Value,
}

/// Input for one control-journal append.
#[derive(Debug, Clone)]
pub struct ControlInput {
    pub action: ControlAction,
    pub scope: ControlScope,
    pub session_key: Option<String>,
    pub task_id: Option<Uuid>,
    pub work_item_public_id: Option<String>,
    pub trace_id: Option<Uuid>,
    pub span_id: Option<i64>,
    pub reason: Option<String>,
    pub actor: Option<String>,
    pub attributes: Value,
}

/// One `crucible_trace_counterexamples` row.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct Counterexample {
    pub id: i64,
    pub trace_id: Option<Uuid>,
    pub span_id: Option<i64>,
    pub experiment_id: Option<i64>,
    pub work_item_public_id: Option<String>,
    pub source: String,
    pub verdict: String,
    pub property: Option<String>,
    pub witness_kind: String,
    pub witness: Value,
    pub content: Option<String>,
    pub content_sha256: String,
    pub metrics: Value,
    pub created_at: DateTime<Utc>,
}

/// Input for one counterexample (idempotent on `content_sha256`).
#[derive(Debug, Clone)]
pub struct CounterexampleInput {
    pub trace_id: Option<Uuid>,
    pub span_id: Option<i64>,
    pub experiment_id: Option<i64>,
    pub work_item_public_id: Option<String>,
    pub source: CexSource,
    pub verdict: CexVerdict,
    pub property: Option<String>,
    pub witness_kind: WitnessKind,
    pub witness: Value,
    pub content: Option<String>,
    pub content_sha256: String,
    pub metrics: Value,
}

// ============================================================================
// Write paths (each writes only pgmcp's own trace tables — memory class).
// ============================================================================

const SPAN_COLS: &str = "span_id, trace_id, parent_span_id, kind, name, status, status_message, \
    started_at, ended_at, session_key, task_id, run_trace_id, work_item_public_id, experiment_id, \
    pi_session_id, role, peer, model, event_lo, event_hi, gtype_cursor, frame_depth, orch_state, \
    critic_iteration, critic_phase, attributes, links, created_at";

/// Insert one span and maintain its transitive-closure rows (`{self,self,0}` plus,
/// for a child, `{ancestor, self, depth+1}` for every ancestor of its parent). The
/// two writes run in one transaction so the closure can never lag the span.
pub async fn record_span(pool: &PgPool, input: &SpanInput) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let span_id: i64 = sqlx::query_scalar(
        "INSERT INTO crucible_trace_spans
            (trace_id, parent_span_id, kind, name, status, status_message, ended_at,
             session_key, task_id, run_trace_id, work_item_public_id, experiment_id,
             pi_session_id, role, peer, model, event_lo, event_hi, gtype_cursor, frame_depth,
             orch_state, critic_iteration, critic_phase, attributes, links)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,
                 $21,$22,$23,$24,$25)
         RETURNING span_id",
    )
    .bind(input.trace_id)
    .bind(input.parent_span_id)
    .bind(input.kind.as_str())
    .bind(&input.name)
    .bind(input.status.as_str())
    .bind(input.status_message.as_deref())
    .bind(input.ended_at)
    .bind(input.session_key.as_deref())
    .bind(input.task_id)
    .bind(input.run_trace_id)
    .bind(input.work_item_public_id.as_deref())
    .bind(input.experiment_id)
    .bind(input.pi_session_id.as_deref())
    .bind(input.role.as_deref())
    .bind(input.peer.as_deref())
    .bind(input.model.as_deref())
    .bind(input.event_lo)
    .bind(input.event_hi)
    .bind(input.gtype_cursor)
    .bind(input.frame_depth)
    .bind(input.orch_state)
    .bind(input.critic_iteration)
    .bind(input.critic_phase.as_deref())
    .bind(&input.attributes)
    .bind(&input.links)
    .fetch_one(&mut *tx)
    .await?;

    // Closure: every ancestor of the parent points at this span at depth+1, plus the
    // self-row at depth 0. A NULL parent matches no ancestor rows, so a root span
    // gets only its self-row.
    sqlx::query(
        "INSERT INTO crucible_trace_span_closure (ancestor_id, descendant_id, depth)
            SELECT ancestor_id, $1, depth + 1
              FROM crucible_trace_span_closure
             WHERE descendant_id = $2
         UNION ALL SELECT $1, $1, 0",
    )
    .bind(span_id)
    .bind(input.parent_span_id)
    .execute(&mut *tx)
    .await?;

    // Realtime event (topic=trace): gated to ROOT spans only (a full trace can
    // open thousands of child spans; only roots drive the live Traces pane).
    // Committed in this tx so the event can never precede the span it announces.
    if input.parent_span_id.is_none() {
        crate::realtime::emit_in_tx(
            &mut tx,
            &crate::realtime::RealtimeEvent::trace_append(
                input.trace_id,
                span_id,
                &input.name,
                input.status.as_str(),
            ),
        )
        .await?;
    }

    tx.commit().await?;
    Ok(span_id)
}

/// Close an open span: set its terminal status, message, and `ended_at` (NOW() when
/// not supplied). Returns the updated row, or `Ok(None)` if no such span.
pub async fn close_span(
    pool: &PgPool,
    span_id: i64,
    status: SpanStatus,
    status_message: Option<&str>,
    ended_at: Option<DateTime<Utc>>,
) -> Result<Option<TraceSpan>, sqlx::Error> {
    let sql = format!(
        "UPDATE crucible_trace_spans
            SET status = $2, status_message = $3, ended_at = COALESCE($4, NOW())
          WHERE span_id = $1
          RETURNING {SPAN_COLS}"
    );
    let updated = sqlx::query_as::<_, TraceSpan>(sqlx::AssertSqlSafe(sql))
        .bind(span_id)
        .bind(status.as_str())
        .bind(status_message)
        .bind(ended_at)
        .fetch_optional(pool)
        .await?;

    // Realtime event (topic=trace): a ROOT span closing is a status change the
    // live Traces pane cares about. Own-tx, best-effort (no surrounding tx).
    if let Some(span) = &updated
        && span.parent_span_id.is_none()
    {
        crate::realtime::emit(
            pool,
            &crate::realtime::RealtimeEvent::trace_append(
                span.trace_id,
                span.span_id,
                &span.name,
                span.status.as_str(),
            ),
        )
        .await;
    }

    Ok(updated)
}

/// Append one annotation to a span. Returns the new id.
pub async fn record_annotation(pool: &PgPool, input: &AnnotationInput) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO crucible_trace_events
            (span_id, trace_id, event_kind, severity, message, event_ord, counterexample_id, attributes)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) RETURNING id",
    )
    .bind(input.span_id)
    .bind(input.trace_id)
    .bind(input.event_kind.as_str())
    .bind(input.severity.as_str())
    .bind(input.message.as_deref())
    .bind(input.event_ord)
    .bind(input.counterexample_id)
    .bind(&input.attributes)
    .fetch_one(pool)
    .await
}

/// Append one control-plane event to the append-only journal. Returns the new id.
pub async fn record_control(pool: &PgPool, input: &ControlInput) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO crucible_control_journal
            (action, scope, session_key, task_id, work_item_public_id, trace_id, span_id, reason, actor, attributes)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING id",
    )
    .bind(input.action.as_str())
    .bind(input.scope.as_str())
    .bind(input.session_key.as_deref())
    .bind(input.task_id)
    .bind(input.work_item_public_id.as_deref())
    .bind(input.trace_id)
    .bind(input.span_id)
    .bind(input.reason.as_deref())
    .bind(input.actor.as_deref())
    .bind(&input.attributes)
    .fetch_one(pool)
    .await
}

/// Persist a counterexample witness; **idempotent** on `content_sha256` (re-recording
/// the same witness returns the existing id). Returns the row id.
pub async fn record_counterexample(
    pool: &PgPool,
    input: &CounterexampleInput,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO crucible_trace_counterexamples
            (trace_id, span_id, experiment_id, work_item_public_id, source, verdict, property,
             witness_kind, witness, content, content_sha256, metrics)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
         ON CONFLICT (content_sha256) DO UPDATE SET
            trace_id = COALESCE(crucible_trace_counterexamples.trace_id, EXCLUDED.trace_id),
            span_id  = COALESCE(crucible_trace_counterexamples.span_id, EXCLUDED.span_id)
         RETURNING id",
    )
    .bind(input.trace_id)
    .bind(input.span_id)
    .bind(input.experiment_id)
    .bind(input.work_item_public_id.as_deref())
    .bind(input.source.as_str())
    .bind(input.verdict.as_str())
    .bind(input.property.as_deref())
    .bind(input.witness_kind.as_str())
    .bind(&input.witness)
    .bind(input.content.as_deref())
    .bind(&input.content_sha256)
    .bind(&input.metrics)
    .fetch_one(pool)
    .await
}

// ============================================================================
// Read paths (pure SELECTs).
// ============================================================================

/// All spans of a trace, ordered for a timeline (start-time, then id).
pub async fn load_spans(pool: &PgPool, trace_id: Uuid) -> Result<Vec<TraceSpan>, sqlx::Error> {
    let sql = format!(
        "SELECT {SPAN_COLS} FROM crucible_trace_spans
          WHERE trace_id = $1 ORDER BY started_at ASC, span_id ASC"
    );
    sqlx::query_as::<_, TraceSpan>(sqlx::AssertSqlSafe(sql))
        .bind(trace_id)
        .fetch_all(pool)
        .await
}

/// All spans of a trace resolved from a `session_key` (joins `orchestration_sessions`
/// → spans by `session_key`).
pub async fn load_spans_by_session(
    pool: &PgPool,
    session_key: &str,
) -> Result<Vec<TraceSpan>, sqlx::Error> {
    let sql = format!(
        "SELECT {SPAN_COLS} FROM crucible_trace_spans
          WHERE session_key = $1 ORDER BY started_at ASC, span_id ASC"
    );
    sqlx::query_as::<_, TraceSpan>(sqlx::AssertSqlSafe(sql))
        .bind(session_key)
        .fetch_all(pool)
        .await
}

/// Annotations of a trace (for the timeline), ordered by time.
pub async fn load_annotations(
    pool: &PgPool,
    trace_id: Uuid,
) -> Result<Vec<TraceAnnotation>, sqlx::Error> {
    sqlx::query_as::<_, TraceAnnotation>(
        "SELECT id, span_id, trace_id, at, event_kind, severity, message, event_ord,
                counterexample_id, attributes
           FROM crucible_trace_events WHERE trace_id = $1 ORDER BY at ASC, id ASC",
    )
    .bind(trace_id)
    .fetch_all(pool)
    .await
}

/// Filters for [`query_spans`] — every field is optional (an absent filter is "any").
#[derive(Debug, Clone, Default)]
pub struct SpanQuery {
    pub kind: Option<String>,
    pub status: Option<String>,
    pub role: Option<String>,
    pub model: Option<String>,
    pub work_item_public_id: Option<String>,
    pub experiment_id: Option<i64>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub limit: i64,
}

/// Cross-trace span filter (the `crucible_trace_query` tool). Newest-first.
pub async fn query_spans(pool: &PgPool, q: &SpanQuery) -> Result<Vec<TraceSpan>, sqlx::Error> {
    let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new("SELECT ");
    qb.push(SPAN_COLS)
        .push(" FROM crucible_trace_spans WHERE TRUE");
    if let Some(k) = &q.kind {
        qb.push(" AND kind = ").push_bind(k.clone());
    }
    if let Some(s) = &q.status {
        qb.push(" AND status = ").push_bind(s.clone());
    }
    if let Some(r) = &q.role {
        qb.push(" AND role = ").push_bind(r.clone());
    }
    if let Some(m) = &q.model {
        qb.push(" AND model = ").push_bind(m.clone());
    }
    if let Some(w) = &q.work_item_public_id {
        qb.push(" AND work_item_public_id = ").push_bind(w.clone());
    }
    if let Some(e) = q.experiment_id {
        qb.push(" AND experiment_id = ").push_bind(e);
    }
    if let Some(t) = q.since {
        qb.push(" AND started_at >= ").push_bind(t);
    }
    if let Some(t) = q.until {
        qb.push(" AND started_at <= ").push_bind(t);
    }
    qb.push(" ORDER BY started_at DESC, span_id DESC LIMIT ")
        .push_bind(q.limit.clamp(1, 10_000));
    qb.build_query_as::<TraceSpan>().fetch_all(pool).await
}

/// Filters for [`load_control_journal`].
#[derive(Debug, Clone, Default)]
pub struct ControlQuery {
    pub action: Option<String>,
    pub scope: Option<String>,
    pub session_key: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub limit: i64,
}

/// The control-plane audit history (the `crucible_trace_audit` tool). Newest-first.
pub async fn load_control_journal(
    pool: &PgPool,
    q: &ControlQuery,
) -> Result<Vec<ControlJournalEntry>, sqlx::Error> {
    let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        "SELECT id, action, scope, session_key, task_id, work_item_public_id, trace_id, span_id, \
         reason, actor, at, attributes FROM crucible_control_journal WHERE TRUE",
    );
    if let Some(a) = &q.action {
        qb.push(" AND action = ").push_bind(a.clone());
    }
    if let Some(s) = &q.scope {
        qb.push(" AND scope = ").push_bind(s.clone());
    }
    if let Some(k) = &q.session_key {
        qb.push(" AND session_key = ").push_bind(k.clone());
    }
    if let Some(t) = q.since {
        qb.push(" AND at >= ").push_bind(t);
    }
    qb.push(" ORDER BY at DESC, id DESC LIMIT ")
        .push_bind(q.limit.clamp(1, 10_000));
    qb.build_query_as::<ControlJournalEntry>()
        .fetch_all(pool)
        .await
}

/// Fetch a counterexample by id, or the latest for a trace (optionally a `source`).
pub async fn load_counterexample(
    pool: &PgPool,
    id: Option<i64>,
    trace_id: Option<Uuid>,
    source: Option<&str>,
) -> Result<Option<Counterexample>, sqlx::Error> {
    const COLS: &str = "id, trace_id, span_id, experiment_id, work_item_public_id, source, verdict, \
        property, witness_kind, witness, content, content_sha256, metrics, created_at";
    if let Some(id) = id {
        let sql = format!("SELECT {COLS} FROM crucible_trace_counterexamples WHERE id = $1");
        return sqlx::query_as::<_, Counterexample>(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .fetch_optional(pool)
            .await;
    }
    let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new("SELECT ");
    qb.push(COLS)
        .push(" FROM crucible_trace_counterexamples WHERE TRUE");
    if let Some(t) = trace_id {
        qb.push(" AND trace_id = ").push_bind(t);
    }
    if let Some(s) = source {
        qb.push(" AND source = ").push_bind(s.to_string());
    }
    qb.push(" ORDER BY created_at DESC, id DESC LIMIT 1");
    qb.build_query_as::<Counterexample>()
        .fetch_optional(pool)
        .await
}

/// Trace header summary (the `crucible_trace_get` tool): counts the run rolls up.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct TraceHeader {
    pub trace_id: Uuid,
    pub n_spans: i64,
    pub n_errors: i64,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub session_key: Option<String>,
    pub work_item_public_id: Option<String>,
}

/// Summarize a trace (counts + bounds + the run/session linkage).
pub async fn trace_header(
    pool: &PgPool,
    trace_id: Uuid,
) -> Result<Option<TraceHeader>, sqlx::Error> {
    sqlx::query_as::<_, TraceHeader>(
        "SELECT $1::uuid AS trace_id,
                COUNT(*)::bigint AS n_spans,
                COUNT(*) FILTER (WHERE status = 'error')::bigint AS n_errors,
                MIN(started_at) AS started_at,
                MAX(ended_at) AS ended_at,
                MAX(session_key) AS session_key,
                MAX(work_item_public_id) AS work_item_public_id
           FROM crucible_trace_spans WHERE trace_id = $1
          HAVING COUNT(*) > 0",
    )
    .bind(trace_id)
    .fetch_optional(pool)
    .await
}

/// The recorded `csm_run_traces.events` for a task (the durable conformance trace),
/// or an empty vec when no trace row exists yet. This is the authoritative event
/// sequence replay/reconcile run against; a paused session's unflushed tail lives on
/// `orchestration_sessions.transcript` ([`crate::csm::session_store::SessionCheckpoint::transcript_events`]).
pub async fn load_run_events(pool: &PgPool, task_id: Uuid) -> Result<Vec<Event>, sqlx::Error> {
    let row: Option<Value> = sqlx::query_scalar(
        "SELECT events FROM csm_run_traces WHERE task_id = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await?;
    Ok(row
        .and_then(|v| serde_json::from_value::<Vec<Event>>(v).ok())
        .unwrap_or_default())
}

/// Resolve the `session_key` a trace was recorded under (the first span carrying one),
/// so the replay tools can load the run's `global_type` from `orchestration_sessions`.
pub async fn session_key_for_trace(
    pool: &PgPool,
    trace_id: Uuid,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT session_key FROM crucible_trace_spans
          WHERE trace_id = $1 AND session_key IS NOT NULL
          ORDER BY span_id ASC LIMIT 1",
    )
    .bind(trace_id)
    .fetch_optional(pool)
    .await
    .map(|o| o.flatten())
}

// ============================================================================
// Pure replay / reconcile (no DB) — the heart of replay / reconcile / why.
// ============================================================================

/// Where an observed run first left the planned protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Divergence {
    /// Event `step` is illegal at the cursor it was replayed from (a driver / role
    /// binding defect). `detail` is the underlying step error message.
    OffProtocol {
        step: usize,
        role: String,
        detail: String,
    },
    /// The trace replayed cleanly but ended before `role` reached a terminal state
    /// (the run stalled / terminated short — deadlock or orphaned worker).
    Stall { role: String, state: usize },
    /// The trace ended with `role` holding `depth` unreturned call frames.
    Unbalanced { role: String, depth: usize },
}

/// Replay `trace` against the planned `net` and return the FIRST divergence, or
/// `None` if the run is conformant (every event legal AND every role terminal with
/// an empty stack). `orchestrator` is preferred when reporting a stall so the
/// surfaced role is the one the operator drives. Pure — reuses
/// [`replay_to_configs`] unchanged (no new soundness surface).
pub fn first_divergence(net: &Network, trace: &[Event], orchestrator: &Role) -> Option<Divergence> {
    match replay_to_configs(net, trace) {
        Err(ConformanceError::Step { ord, role, err }) => Some(Divergence::OffProtocol {
            step: ord,
            role,
            detail: err.message(),
        }),
        Err(ConformanceError::UnknownRole { role, ord }) => Some(Divergence::OffProtocol {
            step: ord,
            role,
            detail: "event names a role with no machine in the network".to_string(),
        }),
        Err(ConformanceError::DepthExceeded { role, ord }) => Some(Divergence::OffProtocol {
            step: ord,
            role,
            detail: "call nesting exceeds MAX_STACK_DEPTH".to_string(),
        }),
        Err(ConformanceError::Unbalanced { role, depth }) => {
            Some(Divergence::Unbalanced { role, depth })
        }
        Err(ConformanceError::Incomplete { role, state }) => {
            // replay_to_configs never returns Incomplete, but map defensively.
            Some(Divergence::Stall { role, state })
        }
        Ok(configs) => {
            // Clean replay: report the first non-terminal role (orchestrator first)
            // or an unreturned stack — else the run is conformant.
            let check = |role: &Role| -> Option<Divergence> {
                let cfg = configs.get(role)?;
                if !cfg.stack.is_empty() {
                    return Some(Divergence::Unbalanced {
                        role: role.to_string(),
                        depth: cfg.stack.len(),
                    });
                }
                let m = net.machine(role)?;
                if !m.is_terminal(cfg.state) {
                    return Some(Divergence::Stall {
                        role: role.to_string(),
                        state: cfg.state,
                    });
                }
                None
            };
            if let Some(d) = check(orchestrator) {
                return Some(d);
            }
            for role in configs.keys() {
                if role == orchestrator {
                    continue;
                }
                if let Some(d) = check(role) {
                    return Some(d);
                }
            }
            None
        }
    }
}

/// The next move for the orchestrator at a replayed position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NextMove {
    /// A single prescribed step `O→peer:request . peer→O:response`.
    Step {
        peer: String,
        request: String,
        response: String,
    },
    /// The orchestrator faces a runtime choice (the Critic loop pass/revise).
    AwaitChoice,
    /// The orchestrator reached a terminal state.
    Terminal,
}

/// One role's recovered configuration at a replayed prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoleState {
    pub role: String,
    pub state: usize,
    pub stack_depth: usize,
    pub terminal: bool,
}

/// The full replayed position of a run prefix (the `crucible_trace_replay` result).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplayPosition {
    pub per_role: Vec<RoleState>,
    pub orch_state: Option<usize>,
    pub orch_frame_depth: Option<usize>,
    pub next: NextMove,
    pub conformant_prefix: bool,
}

/// Replay a run *prefix* and recover the per-role position + the orchestrator's next
/// move — the `crucible_trace_replay` core. `Err(Divergence)` when the prefix is
/// off-protocol (replay **refuses loudly**, the ADR-011 anti-desync posture). Pure.
pub fn replay_position(
    net: &Network,
    prefix: &[Event],
    orchestrator: &Role,
) -> Result<ReplayPosition, Divergence> {
    let configs = match replay_to_configs(net, prefix) {
        Ok(c) => c,
        Err(e) => {
            // Surface the same shape as first_divergence for a corrupt prefix.
            return Err(match e {
                ConformanceError::Step { ord, role, err } => Divergence::OffProtocol {
                    step: ord,
                    role,
                    detail: err.message(),
                },
                ConformanceError::UnknownRole { role, ord } => Divergence::OffProtocol {
                    step: ord,
                    role,
                    detail: "unknown role".to_string(),
                },
                ConformanceError::DepthExceeded { role, ord } => Divergence::OffProtocol {
                    step: ord,
                    role,
                    detail: "depth exceeded".to_string(),
                },
                ConformanceError::Unbalanced { role, depth } => {
                    Divergence::Unbalanced { role, depth }
                }
                ConformanceError::Incomplete { role, state } => Divergence::Stall { role, state },
            });
        }
    };

    let per_role: Vec<RoleState> = configs
        .iter()
        .map(|(r, c)| RoleState {
            role: r.to_string(),
            state: c.state,
            stack_depth: c.stack.len(),
            terminal: net
                .machine(r)
                .map(|m| m.is_terminal(c.state))
                .unwrap_or(false),
        })
        .collect();

    let orch_cfg = configs.get(orchestrator);
    let orch_state = orch_cfg.map(|c| c.state);
    let orch_frame_depth = orch_cfg.map(|c| c.stack.len());

    let next = match orch_state {
        None => NextMove::Terminal,
        Some(st) => {
            let m = net.machine(orchestrator);
            if m.map(|m| m.is_terminal(st)).unwrap_or(true) {
                NextMove::Terminal
            } else {
                match ProtocolDriver::next_step_from(net, orchestrator, st) {
                    Some(step) => NextMove::Step {
                        peer: step.peer.to_string(),
                        request: step.request.name,
                        response: step.response.name,
                    },
                    None => NextMove::AwaitChoice,
                }
            }
        }
    };

    Ok(ReplayPosition {
        per_role,
        orch_state,
        orch_frame_depth,
        next,
        conformant_prefix: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csm::role::Label;
    use std::collections::HashSet;

    // ── vocabulary golden tests (the DB CHECKs are built from these) ──────────
    macro_rules! pin_vocab {
        ($test:ident, $ty:ident, [$($s:literal),+]) => {
            #[test]
            fn $test() {
                let got: HashSet<&str> = $ty::ALL.iter().map(|k| k.as_str()).collect();
                let want: HashSet<&str> = [$($s),+].into_iter().collect();
                assert_eq!(got, want, concat!(stringify!($ty), " vocabulary drifted"));
                for k in $ty::ALL {
                    assert_eq!($ty::parse(k.as_str()), Some(*k));
                    assert!($ty::sql_in_list().contains(&format!("'{}'", k.as_str())));
                }
                assert_eq!($ty::parse("definitely-not-a-variant"), None);
            }
        };
    }
    pin_vocab!(
        span_kind_pinned,
        SpanKind,
        [
            "run",
            "plan",
            "synthesize",
            "fv_gate",
            "planned_step",
            "call_frame",
            "critic_iteration",
            "tool_call",
            "red_team",
            "validate"
        ]
    );
    pin_vocab!(
        span_status_pinned,
        SpanStatus,
        ["unset", "ok", "error", "canceled"]
    );
    pin_vocab!(
        annotation_kind_pinned,
        AnnotationKind,
        [
            "model_chosen",
            "retry",
            "failure",
            "counterexample_found",
            "fv_verdict",
            "critic_verdict",
            "halt",
            "resume",
            "cancel",
            "lease_lost",
            "conformance_fail",
            "off_protocol"
        ]
    );
    pin_vocab!(
        annotation_severity_pinned,
        AnnotationSeverity,
        ["info", "warn", "error"]
    );
    pin_vocab!(
        control_action_pinned,
        ControlAction,
        [
            "halt",
            "resume",
            "checkpoint",
            "cancel",
            "fork",
            "lease_expire",
            "power_fail"
        ]
    );
    pin_vocab!(
        control_scope_pinned,
        ControlScope,
        ["fleet", "session", "task", "work_item"]
    );
    pin_vocab!(
        cex_source_pinned,
        CexSource,
        [
            "tlc",
            "smt",
            "rocq",
            "sfa",
            "presburger",
            "kat",
            "conformance",
            "behavioral"
        ]
    );
    pin_vocab!(
        cex_verdict_pinned,
        CexVerdict,
        ["violated", "sat", "unsat_core", "timeout"]
    );
    pin_vocab!(
        witness_kind_pinned,
        WitnessKind,
        [
            "event_trace",
            "state_assignment",
            "smt_model",
            "tla_trace",
            "proof_term"
        ]
    );

    // ── pure replay / reconcile (no DB) ──────────────────────────────────────
    //
    // The planned protocol is the GOOD 2-worker chain the synth tool folds for a
    // 2-task plan (O→W0:t0_req . W0→O:t0_done . O→W1:t1_req . W1→O:t1_done . end).
    // The crucible/examples/trace fixtures replay the SAME shape against the real
    // Network here.

    fn good_chain() -> (Network, Role) {
        use crate::csm::mpst::global;
        let o = Role::new("O");
        let g = global::interaction(
            o.clone(),
            Role::new("W0"),
            Label::text("t0_req"),
            global::interaction(
                Role::new("W0"),
                o.clone(),
                Label::text("t0_done"),
                global::interaction(
                    o.clone(),
                    Role::new("W1"),
                    Label::text("t1_req"),
                    global::interaction(
                        Role::new("W1"),
                        o.clone(),
                        Label::text("t1_done"),
                        global::end(),
                    ),
                ),
            ),
        );
        (Network::build("trace_test", &g).expect("builds"), o)
    }

    #[test]
    fn conformant_run_has_no_divergence() {
        let (net, o) = good_chain();
        let trace = vec![
            Event::new("O", "W0", Label::text("t0_req")),
            Event::new("W0", "O", Label::text("t0_done")),
            Event::new("O", "W1", Label::text("t1_req")),
            Event::new("W1", "O", Label::text("t1_done")),
        ];
        assert_eq!(first_divergence(&net, &trace, &o), None);
    }

    #[test]
    fn dependency_order_is_off_protocol_at_step_1() {
        // t1_req before t0_done: at cursor after t0_req the plan enables only t0_done,
        // so the second event (index 1) is off-protocol — exactly the dep-order
        // scenario in crucible/examples/trace/dep-order.
        let (net, o) = good_chain();
        let trace = vec![
            Event::new("O", "W0", Label::text("t0_req")),
            Event::new("O", "W1", Label::text("t1_req")),
        ];
        match first_divergence(&net, &trace, &o) {
            Some(Divergence::OffProtocol { step, .. }) => assert_eq!(step, 1),
            other => panic!("expected off_protocol at step 1, got {other:?}"),
        }
    }

    #[test]
    fn premature_end_is_a_stall_at_the_orchestrator() {
        // The run drove the first worker round then stopped: a clean prefix that
        // leaves O mid-protocol — the deadlock/orphan "stall" shape.
        let (net, o) = good_chain();
        let trace = vec![
            Event::new("O", "W0", Label::text("t0_req")),
            Event::new("W0", "O", Label::text("t0_done")),
        ];
        match first_divergence(&net, &trace, &o) {
            Some(Divergence::Stall { role, .. }) => assert_eq!(role, "O"),
            other => panic!("expected a stall at O, got {other:?}"),
        }
    }

    #[test]
    fn replay_position_recovers_state_and_next_step() {
        // Replaying the first worker round recovers O's position and prescribes the
        // next step (the t1_req request) — the resume/step-debug primitive.
        let (net, o) = good_chain();
        let prefix = vec![
            Event::new("O", "W0", Label::text("t0_req")),
            Event::new("W0", "O", Label::text("t0_done")),
        ];
        let pos = replay_position(&net, &prefix, &o).expect("prefix replays");
        assert!(pos.conformant_prefix);
        match pos.next {
            NextMove::Step { peer, request, .. } => {
                assert_eq!(peer, "W1");
                assert_eq!(request, "t1_req");
            }
            other => panic!("expected a next step to W1, got {other:?}"),
        }
    }

    #[test]
    fn replay_position_refuses_a_corrupt_prefix_loudly() {
        // An off-protocol prefix makes replay refuse with a Divergence (never a bogus
        // position) — the ADR-011 anti-desync posture.
        let (net, o) = good_chain();
        let corrupt = vec![Event::new("W1", "O", Label::text("t1_done"))];
        let err = replay_position(&net, &corrupt, &o).expect_err("corrupt prefix refused");
        assert!(matches!(err, Divergence::OffProtocol { step: 0, .. }));
    }
}
