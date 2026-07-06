//! Token-gated operator WRITE handlers for the work-item / bug tracker (ADR-034
//! admin-console amendment). Four endpoints on the `webui_api` sub-router — so
//! all are token + origin gated by `require_webui_auth` — plus the `[webui]
//! writes_enabled` kill-switch:
//!
//! - `POST  /api/work_items/{public_id}/transition` — status transition
//! - `POST  /api/work_items/{public_id}/triage`     — record severity + repro
//! - `POST  /api/work_items/{public_id}/confirm`    — triage → confirmed
//! - `PATCH /api/work_items/{public_id}`            — edit title/priority/body
//!
//! THE TRUST BOUNDARY. Every status transition runs as [`Actor::User`] (the
//! console holds operator authority via `require_webui_auth`) through the SAME
//! `set_work_item_status_in_tx` chokepoint the MCP tools use. The transition
//! matrix (`src/tracker/transition.rs`) has NO `User` arm into `verified` or
//! `rejected` (Gatekeeper/CI-only), so an operator can NEVER self-verify or
//! self-reject: `check_transition` refuses, and [`op_err_to_http`] surfaces that
//! refusal verbatim as HTTP 403. The endpoint NEVER special-cases or bypasses
//! the matrix — it is the single source of truth, and its verdict becomes the
//! HTTP status.
//!
//! `set_work_item_status_in_tx` already emits the `tracker` realtime event in
//! its transaction, so the transition/confirm handlers add ONLY the
//! `webui_audit_log` row (no double-emit). The field-edit paths (triage/patch)
//! use queries that do NOT self-emit, so they emit a `tracker_update` event
//! themselves. All three facts commit in one transaction (ADR-021 in-tx
//! posture).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use super::ApiState;
use super::audit::{AuditAction, AuditEntry, audit_write_tx};
use super::operator::{
    OPERATOR, OptionalPeer, map_db_500, operator_pool, request_ip, writes_enabled_or_403,
};
use crate::db::queries::{
    BugDetailFields, BugDetailsRow, WorkItemOpError, WorkItemRow, fetch_bug_details,
    get_work_item_by_public_id, set_work_item_status_in_tx, update_work_item_fields_in_tx,
    upsert_bug_details_in_tx,
};
use crate::realtime::{RealtimeEvent, emit_in_tx};
use crate::tracker::severity::Severity;
use crate::tracker::status::WorkItemStatus;
use crate::tracker::transition::{Actor, TransitionError};

/// One work item under the `item` key — the shared success envelope, matching
/// the MCP tracker tools' `{"item": ...}` result shape.
#[derive(Debug, Serialize)]
pub struct ItemResponse {
    pub item: WorkItemRow,
}

/// Trim to a non-empty owned string, else `None` (COALESCE "leave unchanged").
fn nonblank(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// Map a tracker op result to an HTTP status. THE TRUST-BOUNDARY SEAM: every
/// refused transition — `Unauthorized` (the operator=User boundary: cannot reach
/// verified/rejected), `Illegal` (no such edge), or an evidence/negotiation gate
/// — is surfaced verbatim as 403. The matrix is never bypassed; its verdict
/// becomes the status. A same-status `NoOp` is a 409 (idempotent conflict); a
/// missing item is 404; a DB fault is 500.
fn op_err_to_http(e: WorkItemOpError) -> (StatusCode, String) {
    match e {
        WorkItemOpError::NotFound => (StatusCode::NOT_FOUND, e.to_string()),
        WorkItemOpError::Db(_) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        WorkItemOpError::Transition(TransitionError::NoOp { .. }) => {
            (StatusCode::CONFLICT, e.to_string())
        }
        WorkItemOpError::Transition(_) => (StatusCode::FORBIDDEN, e.to_string()),
    }
}

// ============================================================================
// POST /api/work_items/{public_id}/transition — operator status transition
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct TransitionRequest {
    pub to_status: String,
    #[serde(default)]
    pub reason: Option<String>,
}

pub(crate) async fn transition_work_item(
    State(state): State<ApiState>,
    peer: OptionalPeer,
    Path(public_id): Path<String>,
    Json(req): Json<TransitionRequest>,
) -> Result<Json<ItemResponse>, (StatusCode, String)> {
    writes_enabled_or_403(&state)?;
    let pool = operator_pool(&state)?;

    // An unparseable status string is a 400 (before we ever touch the matrix);
    // a valid-but-illegal transition is the matrix's call → 403 (below).
    let to = WorkItemStatus::parse(req.to_status.trim()).ok_or_else(|| {
        let allowed = WorkItemStatus::ALL
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        (
            StatusCode::BAD_REQUEST,
            format!(
                "unknown to_status '{}'; expected one of {allowed}",
                req.to_status
            ),
        )
    })?;
    let reason = nonblank(req.reason.as_deref());

    let before = get_work_item_by_public_id(pool, &public_id)
        .await
        .map_err(map_db_500)?
        .ok_or((StatusCode::NOT_FOUND, format!("no work item '{public_id}'")))?;
    let ip = request_ip(peer);

    let mut tx = pool.begin().await.map_err(map_db_500)?;
    // Operator authority = Actor::User. The matrix refuses User → verified /
    // rejected; op_err_to_http turns that into 403. NOT bypassed.
    let updated = set_work_item_status_in_tx(
        &mut tx,
        before.id,
        to,
        Actor::User,
        Some(OPERATOR),
        reason.as_deref(),
        None,
        None,
    )
    .await
    .map_err(op_err_to_http)?;

    // set_work_item_status_in_tx already emitted the tracker realtime event in
    // THIS tx — do NOT double-emit; add only the audit row.
    audit_write_tx(
        &mut tx,
        &AuditEntry {
            actor: OPERATOR.to_string(),
            action: AuditAction::WorkItemTransition,
            target_kind: Some("work_item".to_string()),
            target_id: Some(before.public_id.clone()),
            request_ip: ip,
            before: Some(serde_json::json!({ "status": before.status.clone() })),
            after: Some(serde_json::json!({ "status": updated.status.clone() })),
            reason,
            ok: true,
            error: None,
        },
    )
    .await
    .map_err(map_db_500)?;

    tx.commit().await.map_err(map_db_500)?;
    Ok(Json(ItemResponse { item: updated }))
}

// ============================================================================
// POST /api/work_items/{public_id}/triage — record severity + reproduction
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct TriageRequest {
    pub severity: String,
    pub reproduction_steps: String,
    #[serde(default)]
    pub root_cause: Option<String>,
}

/// One bug's spine + sidecar after a triage record.
#[derive(Debug, Serialize)]
pub struct TriageResponse {
    pub item: WorkItemRow,
    pub bug_details: Option<BugDetailsRow>,
}

pub(crate) async fn triage_work_item(
    State(state): State<ApiState>,
    peer: OptionalPeer,
    Path(public_id): Path<String>,
    Json(req): Json<TriageRequest>,
) -> Result<Json<TriageResponse>, (StatusCode, String)> {
    writes_enabled_or_403(&state)?;
    let pool = operator_pool(&state)?;

    let item = get_work_item_by_public_id(pool, &public_id)
        .await
        .map_err(map_db_500)?
        .ok_or((StatusCode::NOT_FOUND, format!("no work item '{public_id}'")))?;
    if item.kind != "bug" {
        return Err((
            StatusCode::BAD_REQUEST,
            "triage applies to bugs (kind='bug'); use PATCH /api/work_items/{id} for other kinds"
                .to_string(),
        ));
    }
    let severity = Severity::parse(req.severity.trim()).ok_or((
        StatusCode::BAD_REQUEST,
        format!(
            "unknown severity '{}'; expected one of {}",
            req.severity.trim(),
            crate::tracker::severity::sql_in_list()
        ),
    ))?;
    let reproduction = req.reproduction_steps.trim();
    if reproduction.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "reproduction_steps must be non-empty".to_string(),
        ));
    }
    let root_cause = nonblank(req.root_cause.as_deref());
    let ip = request_ip(peer);

    let mut tx = pool.begin().await.map_err(map_db_500)?;

    // Set severity on the spine; seed a priority ONLY when the item still carries
    // the default (0) — never clobber an explicit one (mirrors work_item_triage).
    let derived_priority = (item.priority == 0).then(|| severity.default_priority());
    let updated = update_work_item_fields_in_tx(
        &mut tx,
        item.id,
        None,
        None,
        derived_priority,
        None,
        None,
        false,
        None,
        false,
        Some(severity.as_str()),
    )
    .await
    .map_err(op_err_to_http)?;

    // Record the reproduction / root-cause + stamp the triage milestone.
    upsert_bug_details_in_tx(
        &mut tx,
        item.id,
        &BugDetailFields {
            reproduction_steps: Some(reproduction),
            root_cause: root_cause.as_deref(),
            triaged_by: Some(OPERATOR),
            set_triaged_at: true,
            ..Default::default()
        },
    )
    .await
    .map_err(map_db_500)?;

    // These field-update queries do NOT self-emit (unlike the status chokepoint),
    // so emit the non-status tracker_update event ourselves, in this tx.
    emit_in_tx(
        &mut tx,
        &RealtimeEvent::tracker_update(
            &updated.public_id,
            &updated.title,
            &updated.status,
            &updated.kind,
            updated.project_id,
        ),
    )
    .await
    .map_err(map_db_500)?;

    audit_write_tx(
        &mut tx,
        &AuditEntry {
            actor: OPERATOR.to_string(),
            action: AuditAction::WorkItemTriage,
            target_kind: Some("work_item".to_string()),
            target_id: Some(item.public_id.clone()),
            request_ip: ip,
            before: Some(serde_json::json!({ "severity": item.severity.clone() })),
            after: Some(serde_json::json!({
                "severity": severity.as_str(),
                "reproduction_recorded": true,
                "root_cause_recorded": root_cause.is_some(),
                "triaged_by": OPERATOR,
            })),
            reason: None,
            ok: true,
            error: None,
        },
    )
    .await
    .map_err(map_db_500)?;

    tx.commit().await.map_err(map_db_500)?;

    let bug_details = fetch_bug_details(pool, item.id).await.map_err(map_db_500)?;
    Ok(Json(TriageResponse {
        item: updated,
        bug_details,
    }))
}

// ============================================================================
// POST /api/work_items/{public_id}/confirm — triage → confirmed (User-only)
// ============================================================================

#[derive(Debug, Default, Deserialize)]
pub struct ConfirmRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

pub(crate) async fn confirm_work_item(
    State(state): State<ApiState>,
    peer: OptionalPeer,
    Path(public_id): Path<String>,
    body: Option<Json<ConfirmRequest>>,
) -> Result<Json<ItemResponse>, (StatusCode, String)> {
    writes_enabled_or_403(&state)?;
    let pool = operator_pool(&state)?;
    let reason = body.and_then(|b| nonblank(b.0.reason.as_deref()));

    let item = get_work_item_by_public_id(pool, &public_id)
        .await
        .map_err(map_db_500)?
        .ok_or((StatusCode::NOT_FOUND, format!("no work item '{public_id}'")))?;
    if item.kind != "bug" {
        return Err((
            StatusCode::BAD_REQUEST,
            "confirm applies to bugs (kind='bug')".to_string(),
        ));
    }
    // A bug is not confirmable without a severity + reproduction (mirror the
    // work_item_triage tool's guard). POST /triage first to record them.
    if item.severity.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            "a severity must be set before confirming — POST /api/work_items/{id}/triage first"
                .to_string(),
        ));
    }
    let details = fetch_bug_details(pool, item.id).await.map_err(map_db_500)?;
    let has_repro = details
        .as_ref()
        .and_then(|d| d.reproduction_steps.as_deref())
        .is_some_and(|s| !s.trim().is_empty());
    if !has_repro {
        return Err((
            StatusCode::BAD_REQUEST,
            "reproduction steps must be recorded before confirming — POST \
             /api/work_items/{id}/triage first"
                .to_string(),
        ));
    }
    let ip = request_ip(peer);

    let mut tx = pool.begin().await.map_err(map_db_500)?;
    // triage → confirmed is a User-only matrix edge (no Agent arm). The operator
    // holds that authority via the webui token; the matrix refuses anything else
    // (e.g. a not-in-triage item → Illegal → 403 via op_err_to_http).
    let updated = set_work_item_status_in_tx(
        &mut tx,
        item.id,
        WorkItemStatus::Confirmed,
        Actor::User,
        Some(OPERATOR),
        reason.as_deref().or(Some("operator: confirmed")),
        None,
        None,
    )
    .await
    .map_err(op_err_to_http)?;

    // set_work_item_status_in_tx already emitted the tracker event — audit only.
    audit_write_tx(
        &mut tx,
        &AuditEntry {
            actor: OPERATOR.to_string(),
            action: AuditAction::WorkItemConfirm,
            target_kind: Some("work_item".to_string()),
            target_id: Some(item.public_id.clone()),
            request_ip: ip,
            before: Some(serde_json::json!({ "status": item.status.clone() })),
            after: Some(serde_json::json!({ "status": updated.status.clone() })),
            reason,
            ok: true,
            error: None,
        },
    )
    .await
    .map_err(map_db_500)?;

    tx.commit().await.map_err(map_db_500)?;
    Ok(Json(ItemResponse { item: updated }))
}

// ============================================================================
// PATCH /api/work_items/{public_id} — edit title / priority / body (no status)
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct PatchWorkItemRequest {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub priority: Option<i32>,
    #[serde(default)]
    pub body: Option<String>,
    /// Editor-save alias for `body` — the console's inline editor POSTs `{text}`;
    /// when `body` is absent, `text` supplies the new body.
    #[serde(default)]
    pub text: Option<String>,
}

pub(crate) async fn patch_work_item(
    State(state): State<ApiState>,
    peer: OptionalPeer,
    Path(public_id): Path<String>,
    Json(req): Json<PatchWorkItemRequest>,
) -> Result<Json<ItemResponse>, (StatusCode, String)> {
    writes_enabled_or_403(&state)?;
    let pool = operator_pool(&state)?;

    let title = nonblank(req.title.as_deref());
    // `text` is the editor-save alias for `body` (explicit `body` wins).
    let body = nonblank(req.body.as_deref().or(req.text.as_deref()));
    let priority = req.priority;
    if title.is_none() && body.is_none() && priority.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            "no fields to update: supply at least one of title, priority, body (or text)"
                .to_string(),
        ));
    }

    let before = get_work_item_by_public_id(pool, &public_id)
        .await
        .map_err(map_db_500)?
        .ok_or((StatusCode::NOT_FOUND, format!("no work item '{public_id}'")))?;
    let ip = request_ip(peer);

    let mut tx = pool.begin().await.map_err(map_db_500)?;
    // Never touches status — update_work_item_fields_in_tx has no status column
    // in its UPDATE; the trust boundary is untouchable from this path.
    let updated = update_work_item_fields_in_tx(
        &mut tx,
        before.id,
        title.as_deref(),
        body.as_deref(),
        priority,
        None,
        None,
        false,
        None,
        false,
        None,
    )
    .await
    .map_err(op_err_to_http)?;

    emit_in_tx(
        &mut tx,
        &RealtimeEvent::tracker_update(
            &updated.public_id,
            &updated.title,
            &updated.status,
            &updated.kind,
            updated.project_id,
        ),
    )
    .await
    .map_err(map_db_500)?;

    audit_write_tx(
        &mut tx,
        &AuditEntry {
            actor: OPERATOR.to_string(),
            action: AuditAction::WorkItemUpdate,
            target_kind: Some("work_item".to_string()),
            target_id: Some(before.public_id.clone()),
            request_ip: ip,
            before: Some(serde_json::json!({
                "title": before.title.clone(),
                "priority": before.priority,
                "body": before.body.clone(),
            })),
            after: Some(serde_json::json!({
                "title": updated.title.clone(),
                "priority": updated.priority,
                "body": updated.body.clone(),
            })),
            reason: None,
            ok: true,
            error: None,
        },
    )
    .await
    .map_err(map_db_500)?;

    tx.commit().await.map_err(map_db_500)?;
    Ok(Json(ItemResponse { item: updated }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::transition::{TransitionContext, check_transition};

    fn full_ctx() -> TransitionContext {
        TransitionContext {
            evidence_passing: true,
            evidence_present: true,
            user_negotiation: true,
        }
    }

    #[test]
    fn operator_cannot_reach_verified_or_rejected_maps_to_403() {
        // The operator acts as Actor::User. The matrix has NO User arm into
        // verified / rejected (Gatekeeper/CI-only), so check_transition refuses
        // from every DISTINCT source — and the endpoint layer surfaces that
        // verbatim as 403, never bypassing the matrix. (Same-status is a NoOp,
        // a distinct 409 case, so it is excluded here.)
        for &target in &[WorkItemStatus::Verified, WorkItemStatus::Rejected] {
            for from in WorkItemStatus::ALL {
                if *from == target {
                    continue;
                }
                let err = check_transition(*from, target, Actor::User, full_ctx()).expect_err(
                    "operator (Actor::User) must never be permitted into verified/rejected",
                );
                let (code, _) = op_err_to_http(WorkItemOpError::Transition(err));
                assert_eq!(
                    code,
                    StatusCode::FORBIDDEN,
                    "operator {from:?} -> {target:?} must map to 403, got {code}"
                );
            }
        }
    }

    #[test]
    fn op_err_status_mapping_is_stable() {
        assert_eq!(
            op_err_to_http(WorkItemOpError::NotFound).0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            op_err_to_http(WorkItemOpError::Db(sqlx::Error::PoolClosed)).0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            op_err_to_http(WorkItemOpError::Transition(TransitionError::NoOp {
                status: WorkItemStatus::InProgress
            }))
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            op_err_to_http(WorkItemOpError::Transition(TransitionError::Unauthorized {
                from: WorkItemStatus::ClaimedDone,
                to: WorkItemStatus::Verified,
                actor: Actor::User,
            }))
            .0,
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn emitted_audit_actions_are_valid() {
        for a in [
            AuditAction::WorkItemTransition,
            AuditAction::WorkItemTriage,
            AuditAction::WorkItemConfirm,
            AuditAction::WorkItemUpdate,
        ] {
            assert!(
                AuditAction::ALL.contains(&a),
                "{} missing from AuditAction::ALL",
                a.as_str()
            );
        }
    }

    /// A legal operator transition (User is in the actor set and no gate blocks
    /// it) must NOT be an error — proving the 403 mapping is specific to refused
    /// transitions, not a blanket denial of the whole endpoint.
    #[test]
    fn legal_operator_transition_is_permitted_by_matrix() {
        assert!(
            check_transition(
                WorkItemStatus::Triage,
                WorkItemStatus::Confirmed,
                Actor::User,
                full_ctx()
            )
            .is_ok()
        );
        assert!(
            check_transition(
                WorkItemStatus::InProgress,
                WorkItemStatus::Blocked,
                Actor::User,
                full_ctx()
            )
            .is_ok()
        );
    }
}
