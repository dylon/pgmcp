//! Read-only REST handlers backing the web UI **Experiments** pane.
//!
//! These expose the scientific-experiment ledgers (`experiments` and its child
//! tables — hypotheses, runs/samples, decisions, artifacts) over the daemon's
//! HTTP surface. Every handler is read-only and infallible from axum's point of
//! view: it returns `Json<Value>` and encodes any error (DB down, not found)
//! *inside* the body so the pane always receives a well-shaped response.
//!
//! Query shaping mirrors the `experiment_get` / `experiment_list` MCP tools
//! (`crate::mcp::tools::tool_experiments`) so the web UI and an MCP client see
//! the same fields; the query layer is reused wholesale from
//! `crate::db::queries` and the ledger is rendered by the *same* pure renderer
//! the `experiment_render_ledger` tool uses
//! (`crate::experiment::ledger::render_markdown`).
//!
//! Wiring (in `src/cli/daemon.rs`, alongside the other `/api/*` routes):
//! ```ignore
//! .route("/api/experiments",         axum::routing::get(api::experiments::experiments_list))
//! .route("/api/experiments/{slug}",  axum::routing::get(api::experiments::experiment_get))
//! .route("/api/experiments/{slug}/ledger", axum::routing::get(api::experiments::experiment_ledger))
//! ```

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;

use super::ApiState;
use super::audit::{AuditAction, AuditEntry, audit_write_tx};
use super::operator::{
    OPERATOR, OptionalPeer, map_db_500, operator_pool, request_ip, writes_enabled_or_403,
};
use crate::db::queries::{
    ExperimentAssignmentRow, current_realtime_seq, experiment_runs_overview, experiment_timeline,
    get_experiment_assignment_for_update_in_tx, get_experiment_core, list_experiment_artifacts,
    list_experiment_hypotheses, list_experiment_results, list_experiments_webui,
    resolve_project_id, update_experiment_assignment_in_tx,
};
use crate::experiment::vocab::{ExperimentRunStatus, ExperimentStatus};

/// Default / maximum page size for the experiments list.
const LIST_DEFAULT_LIMIT: i64 = 50;
const LIST_MAX_LIMIT: i64 = 200;

// ============================================================================
// GET /api/experiments?project=<opt>&status=<opt>&limit=<opt>
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct ExperimentsQuery {
    /// Filter by human-readable project name (`projects.name`). Unknown ⇒ empty.
    #[serde(default)]
    pub project: Option<String>,
    /// Filter by experiment status (the closed `experiments.status` vocabulary).
    #[serde(default)]
    pub status: Option<String>,
    /// Page size; clamped to `1..=LIST_MAX_LIMIT`, default `LIST_DEFAULT_LIMIT`.
    #[serde(default)]
    pub limit: Option<i64>,
}

/// List experiment ledgers (newest-updated first), each carrying its headline
/// hypothesis and latest-decision rollup. Null pool ⇒ `{experiments:[],server_seq:0}`.
pub async fn experiments_list(
    State(state): State<ApiState>,
    Query(params): Query<ExperimentsQuery>,
) -> Json<Value> {
    let Some(pool) = state.db.pool() else {
        return Json(json!({ "experiments": [], "server_seq": 0 }));
    };

    let project = trimmed(params.project.as_deref());
    let status = trimmed(params.status.as_deref());
    let limit = params
        .limit
        .unwrap_or(LIST_DEFAULT_LIMIT)
        .clamp(1, LIST_MAX_LIMIT);

    let server_seq = server_seq_or_log(pool, "experiments_list").await;

    match list_experiments_webui(pool, project, status, limit).await {
        Ok(rows) => {
            // The row struct derives Serialize; to_value cannot realistically
            // fail here, but ADR-021 forbids a silently-swallowed error.
            let experiments = serde_json::to_value(&rows).unwrap_or_else(|e| {
                tracing::error!(error = %e, "experiments_list: serialize rows failed");
                Value::Array(Vec::new())
            });
            Json(json!({ "experiments": experiments, "server_seq": server_seq }))
        }
        Err(e) => {
            tracing::error!(error = %e, "experiments_list: list_experiments_webui failed");
            Json(json!({ "experiments": [], "server_seq": server_seq }))
        }
    }
}

// ============================================================================
// GET /api/experiments/{slug} — the full ledger record + child tables
// ============================================================================

/// Fetch one experiment by slug together with its hypotheses, measurement runs,
/// decisions, artifacts, and timeline. Null pool / not found / lookup error ⇒
/// an empty-but-well-shaped body with an `error` note (never an HTTP error).
pub async fn experiment_get(
    State(state): State<ApiState>,
    Path(slug): Path<String>,
) -> Json<Value> {
    let Some(pool) = state.db.pool() else {
        return Json(empty_detail(&slug, "database unavailable"));
    };

    let core = match get_experiment_core(pool, None, Some(&slug)).await {
        Ok(Some(core)) => core,
        Ok(None) => return Json(empty_detail(&slug, "experiment not found")),
        Err(e) => {
            tracing::error!(error = %e, slug = %slug, "experiment_get: get_experiment_core failed");
            return Json(empty_detail(&slug, "experiment lookup failed"));
        }
    };

    let hyps = or_log_empty(
        list_experiment_hypotheses(pool, core.id).await,
        "list_experiment_hypotheses",
    );
    let results = or_log_empty(
        list_experiment_results(pool, core.id).await,
        "list_experiment_results",
    );
    let runs = or_log_empty(
        experiment_runs_overview(pool, core.id).await,
        "experiment_runs_overview",
    );
    let artifacts = or_log_empty(
        list_experiment_artifacts(pool, core.id).await,
        "list_experiment_artifacts",
    );
    let events = or_log_empty(
        experiment_timeline(pool, core.id).await,
        "experiment_timeline",
    );

    let server_seq = server_seq_or_log(pool, "experiment_get").await;

    let experiment = json!({
        "id": core.id,
        "slug": core.slug,
        "title": core.title,
        "question": core.question,
        "context": core.context,
        "kind": core.kind,
        "status": core.status,
        "project": core.project,
        "git_ref": core.git_ref,
        "plan_ref": core.plan_ref,
        "correction": core.correction,
        "created_at": core.created_at.to_rfc3339(),
        "updated_at": core.updated_at.to_rfc3339(),
    });

    let hypotheses: Vec<Value> = hyps
        .iter()
        .map(|h| {
            json!({
                "hypothesis_id": h.id,
                "statement": h.statement,
                "primary_metric": h.primary_metric,
                "unit": h.unit,
                "predicted_direction": h.predicted_direction,
                "acceptance_criterion": serde_json::from_str::<Value>(&h.acceptance_criterion_json)
                    .unwrap_or(Value::Null),
                "criterion_locked_at": h.criterion_locked_at.to_rfc3339(),
                "planned_n": h.planned_n,
                "verdict": h.verdict,
            })
        })
        .collect();

    // "measurements" = per-run overview (arm + status + non-warmup sample count +
    // digest). Raw per-replicate samples are intentionally NOT inlined (they can
    // be large); `sample_count` is the pane-appropriate granularity, matching the
    // `experiment_get` MCP tool's `measurement_runs` section.
    let measurements: Vec<Value> = runs
        .iter()
        .map(|r| {
            let usable = ExperimentRunStatus::parse(&r.status)
                .map(|s| s.usable_in_decision())
                .unwrap_or(false);
            json!({
                "run_id": r.run_id,
                "arm_label": r.arm_label,
                "arm_kind": r.arm_kind,
                "status": r.status,
                "usable_in_decision": usable,
                "sample_count": r.sample_count,
                "samples_digest": r.samples_digest,
                "status_reason": r.status_reason,
                "finalized_at": r.finalized_at.map(|t| t.to_rfc3339()),
            })
        })
        .collect();

    let decisions: Vec<Value> = results
        .iter()
        .map(|r| {
            json!({
                "result_id": r.id,
                "hypothesis_id": r.hypothesis_id,
                "test_type": r.test_type,
                "metric": r.metric_name,
                "statistic": r.statistic,
                "p_value": r.p_value,
                "effect_size": r.effect_size,
                "ci_low": r.ci_low,
                "ci_high": r.ci_high,
                "verdict": r.verdict,
                "accepted": r.accepted,
                "rationale": r.rationale,
                "decided_at": r.created_at.to_rfc3339(),
            })
        })
        .collect();

    let artifacts_json: Vec<Value> = artifacts
        .iter()
        .map(|a| {
            json!({
                "artifact_id": a.id,
                "kind": a.kind,
                "tool": a.tool,
                "label": a.label,
                "content": a.content,
                "content_sha256": a.content_sha256,
                "metrics": a.metrics,
                "git_ref": a.git_ref,
                "created_at": a.created_at.to_rfc3339(),
            })
        })
        .collect();

    let timeline: Vec<Value> = events
        .iter()
        .map(|e| {
            json!({
                "at": e.at.to_rfc3339(),
                "event": e.event,
                "detail": e.detail,
            })
        })
        .collect();

    Json(json!({
        "experiment": experiment,
        "hypotheses": hypotheses,
        "measurements": measurements,
        "decisions": decisions,
        "artifacts": artifacts_json,
        "timeline": timeline,
        "server_seq": server_seq,
    }))
}

// ============================================================================
// GET /api/experiments/{slug}/ledger — the rendered markdown scientific ledger
// ============================================================================

/// Render the committed-style markdown ledger for one experiment, REUSING the
/// exact renderer the `experiment_render_ledger` MCP tool uses
/// (`crate::experiment::ledger::render_markdown`). Pure render — no file I/O.
/// Null pool / not found / lookup error ⇒ `{ledger:"", error:"…"}`.
pub async fn experiment_ledger(
    State(state): State<ApiState>,
    Path(slug): Path<String>,
) -> Json<Value> {
    let Some(pool) = state.db.pool() else {
        return Json(json!({ "ledger": "", "error": "database unavailable" }));
    };

    let core = match get_experiment_core(pool, None, Some(&slug)).await {
        Ok(Some(core)) => core,
        Ok(None) => return Json(json!({ "ledger": "", "error": "experiment not found" })),
        Err(e) => {
            tracing::error!(error = %e, slug = %slug, "experiment_ledger: get_experiment_core failed");
            return Json(json!({ "ledger": "", "error": "experiment lookup failed" }));
        }
    };

    let hyps = or_log_empty(
        list_experiment_hypotheses(pool, core.id).await,
        "list_experiment_hypotheses",
    );
    let results = or_log_empty(
        list_experiment_results(pool, core.id).await,
        "list_experiment_results",
    );
    let events = or_log_empty(
        experiment_timeline(pool, core.id).await,
        "experiment_timeline",
    );

    let ledger = crate::experiment::ledger::render_markdown(&core, &hyps, &results, &events);
    Json(json!({ "ledger": ledger, "slug": core.slug }))
}

// ============================================================================
// PATCH /api/experiments/{slug} — operator project/status/title assignment
// ============================================================================

/// Deserialize a field into `Option<Option<T>>`, distinguishing the three JSON
/// states a plain `Option` collapses: ABSENT ⇒ `None` (leave unchanged),
/// `null` ⇒ `Some(None)` (explicitly clear), and a value ⇒ `Some(Some(v))`
/// (set). Paired with `#[serde(default)]` so an omitted key yields the outer
/// `None`. This is what lets `PATCH {"project": null}` CLEAR the assignment
/// while an omitted `project` key leaves it untouched — a plain COALESCE cannot
/// express "set to NULL".
fn double_option<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::<T>::deserialize(de)?))
}

/// Operator edit to one experiment's mutable assignment fields. Every field is
/// optional (omit to leave unchanged); `project` additionally accepts an
/// explicit `null` to CLEAR the assignment (a workspace-general experiment).
#[derive(Debug, Default, Deserialize)]
pub struct UpdateExperimentRequest {
    /// Project NAME (`projects.name`) to assign, JSON `null` to clear, or omit
    /// to leave unchanged. An unknown name is a 400.
    #[serde(default, deserialize_with = "double_option")]
    pub project: Option<Option<String>>,
    /// New status from the closed experiments vocabulary
    /// (open | measuring | decided | abandoned | superseded). Omit to leave.
    #[serde(default)]
    pub status: Option<String>,
    /// New title (non-empty after trim). Omit to leave.
    #[serde(default)]
    pub title: Option<String>,
}

/// The success envelope for the operator assignment write: the updated
/// experiment's editable projection under `experiment`.
#[derive(Debug, Serialize)]
pub struct ExperimentUpdateResponse {
    pub experiment: ExperimentAssignmentRow,
}

/// `PATCH /api/experiments/{slug}` — token-gated (webui_api sub-router) +
/// kill-switch-gated (`[webui] writes_enabled`) operator assignment of an
/// experiment's project / status / title. One transaction: lock+read the
/// pre-image, UPDATE, and write the `webui_audit_log` row (ADR-021 in-tx), so
/// the edit and its audit commit atomically or not at all. Unlike the read
/// handlers (which encode errors in the body), this WRITE returns real HTTP
/// status codes so the console can surface 400/403/404 distinctly.
pub(crate) async fn experiment_update(
    State(state): State<ApiState>,
    peer: OptionalPeer,
    Path(slug): Path<String>,
    Json(req): Json<UpdateExperimentRequest>,
) -> Result<Json<ExperimentUpdateResponse>, (StatusCode, String)> {
    writes_enabled_or_403(&state)?;
    let pool = operator_pool(&state)?;

    // Validate status up-front (400) so an out-of-vocab value never reaches the
    // DB CHECK; a valid one flows to COALESCE below (as a `&'static str`).
    let status = match req
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(s) => Some(
            ExperimentStatus::parse(s)
                .map(|v| v.as_str())
                .ok_or_else(|| {
                    let allowed = ExperimentStatus::ALL
                        .iter()
                        .map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    (
                        StatusCode::BAD_REQUEST,
                        format!("unknown status '{s}'; expected one of {allowed}"),
                    )
                })?,
        ),
        None => None,
    };
    let title = req
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);

    // Resolve the tri-state `project` field into (touched, id). `touched` gates
    // the SET; `id == None` while touched ⇒ clear to NULL.
    let (project_touched, new_project_id) = match &req.project {
        None => (false, None),
        Some(None) => (true, None), // explicit null ⇒ clear
        Some(Some(name)) => {
            let name = name.trim();
            if name.is_empty() {
                (true, None) // empty string ⇒ treat as clear
            } else {
                let id = resolve_project_id(pool, Some(name))
                    .await
                    .map_err(map_db_500)?
                    .ok_or((StatusCode::BAD_REQUEST, format!("unknown project '{name}'")))?;
                (true, Some(id))
            }
        }
    };

    if !project_touched && status.is_none() && title.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            "no fields to update: supply at least one of project, status, title".to_string(),
        ));
    }
    let ip = request_ip(peer);

    let mut tx = pool.begin().await.map_err(map_db_500)?;
    let before = get_experiment_assignment_for_update_in_tx(&mut tx, &slug)
        .await
        .map_err(map_db_500)?
        .ok_or((StatusCode::NOT_FOUND, format!("no experiment '{slug}'")))?;

    let after = update_experiment_assignment_in_tx(
        &mut tx,
        before.id,
        project_touched,
        new_project_id,
        status,
        title.as_deref(),
    )
    .await
    .map_err(map_db_500)?
    .ok_or((
        StatusCode::NOT_FOUND,
        format!("experiment '{slug}' vanished mid-update"),
    ))?;

    audit_write_tx(
        &mut tx,
        &AuditEntry {
            actor: OPERATOR.to_string(),
            action: AuditAction::ExperimentUpdate,
            target_kind: Some("experiment".to_string()),
            target_id: Some(after.slug.clone()),
            request_ip: ip,
            before: Some(json!({
                "project_id": before.project_id,
                "status": before.status,
                "title": before.title,
            })),
            after: Some(json!({
                "project_id": after.project_id,
                "status": after.status,
                "title": after.title,
            })),
            reason: None,
            ok: true,
            error: None,
        },
    )
    .await
    .map_err(map_db_500)?;

    tx.commit().await.map_err(map_db_500)?;
    Ok(Json(ExperimentUpdateResponse { experiment: after }))
}

// ============================================================================
// Helpers
// ============================================================================

/// Trim a query-param string and drop it if empty (so `?project=` ⇒ no filter).
fn trimmed(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

/// The current realtime-event sequence (the web UI's replay cursor), or `0` with
/// an ADR-021 `error!` if the read fails — never a silent swallow.
async fn server_seq_or_log(pool: &PgPool, op: &str) -> i64 {
    match current_realtime_seq(pool).await {
        Ok(seq) => seq,
        Err(e) => {
            tracing::error!(error = %e, op = %op, "current_realtime_seq failed");
            0
        }
    }
}

/// Unwrap a child-table query, logging (ADR-021 `error!`) and degrading to an
/// empty vec on failure so one bad child never fails the whole detail view.
fn or_log_empty<T>(res: Result<Vec<T>, sqlx::Error>, op: &str) -> Vec<T> {
    match res {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, op = %op, "experiment detail child query failed");
            Vec::new()
        }
    }
}

/// The empty-but-well-shaped detail body (stable keys) for the null-pool /
/// not-found / lookup-error paths, carrying an `error` note for the pane.
fn empty_detail(slug: &str, error: &str) -> Value {
    json!({
        "experiment": Value::Null,
        "hypotheses": [],
        "measurements": [],
        "decisions": [],
        "artifacts": [],
        "timeline": [],
        "server_seq": 0,
        "slug": slug,
        "error": error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn experiment_update_audit_action_is_pinned() {
        // Golden: the action this handler writes is in the closed, CHECK-pinned
        // vocabulary (so the v66/v68 constraint can never reject our row).
        assert!(AuditAction::ALL.contains(&AuditAction::ExperimentUpdate));
        assert_eq!(AuditAction::ExperimentUpdate.as_str(), "experiment_update");
    }

    #[test]
    fn project_field_distinguishes_absent_null_and_value() {
        // Absent ⇒ None (leave the assignment unchanged).
        let r: UpdateExperimentRequest = serde_json::from_str("{}").expect("empty body");
        assert_eq!(r.project, None);
        // Explicit null ⇒ Some(None) (clear the assignment).
        let r: UpdateExperimentRequest =
            serde_json::from_str(r#"{"project":null}"#).expect("null project");
        assert_eq!(r.project, Some(None));
        // A value ⇒ Some(Some(name)) (assign by name).
        let r: UpdateExperimentRequest =
            serde_json::from_str(r#"{"project":"pgmcp"}"#).expect("named project");
        assert_eq!(r.project, Some(Some("pgmcp".to_string())));
    }
}
