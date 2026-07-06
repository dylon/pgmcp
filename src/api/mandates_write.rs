//! Token-gated operator WRITE handlers for durable mandates (ADR-034
//! admin-console amendment). Four endpoints on the `webui_api` sub-router — so
//! all are token + origin gated by `require_webui_auth` before they run — plus
//! the `[webui] writes_enabled` kill-switch:
//!
//! - `POST   /api/mandates/durable`             — create an operator rule
//! - `PATCH  /api/mandates/durable/{id}`        — edit an operator rule
//! - `POST   /api/mandates/durable/{id}/retire` — soft-delete (retired_at)
//! - `POST   /api/mandates/promote`             — promote a session mandate
//!
//! EVERY mutation commits three facts in ONE transaction (the ADR-021 in-tx
//! posture): the durable-table change, the `mandate` realtime event
//! (`crate::realtime::emit_in_tx`), and the `webui_audit_log` row
//! (`crate::api::audit::audit_write_tx`). A failed emit/audit aborts the
//! mutation, so the audit trail and the realtime feed can never drift from the
//! table. `promote` reuses `crate::sessions::promote_mandate_in_tx` (which emits
//! the realtime event itself) and adds only the audit row in the same tx.

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
    DurableMandateRow, create_durable_mandate_in_tx, get_durable_mandate_for_update_in_tx,
    retire_durable_mandate_in_tx, update_durable_mandate_in_tx,
};
use crate::realtime::{RealtimeEvent, emit_in_tx};

/// The marker section operator promotions append their bullet under — identical
/// to the MCP `promote_session_mandate` path so both write to the same section.
const PROMOTED_MARKER: &str = "## Promoted session mandates (pgmcp)";

// ============================================================================
// Shared request-shaping
// ============================================================================

/// Validate `scope` against the durable-mandate vocabulary (v65: global adds to
/// the original project|workspace). Lower-cased + trimmed.
fn validate_mandate_scope(raw: &str) -> Result<String, (StatusCode, String)> {
    let scope = raw.trim().to_ascii_lowercase();
    match scope.as_str() {
        "global" | "project" | "workspace" => Ok(scope),
        _ => Err((
            StatusCode::BAD_REQUEST,
            "scope must be one of global, project, workspace".to_string(),
        )),
    }
}

/// Validate `polarity` against the 12-value `MandatePolarity` vocabulary
/// (defensive: `durable_mandates.polarity` has no DB CHECK, so a typo would
/// otherwise land an unconstrained rule). Returns the canonical string.
fn validate_polarity(raw: &str) -> Result<String, (StatusCode, String)> {
    crate::sessions::MandatePolarity::parse(raw.trim())
        .map(|p| p.as_str().to_string())
        .ok_or((
            StatusCode::BAD_REQUEST,
            "polarity must be one of always, never, prefer, avoid, remember, from_now_on, \
             correction, permission, constraint, mandate, process_rule, project_rule"
                .to_string(),
        ))
}

/// Resolve the `project_id` for a scope+project pair. `scope='project'` requires
/// a known project name (400 otherwise); `global`/`workspace` reject a stray
/// project name so a misconfiguration surfaces rather than being silently
/// dropped.
async fn resolve_scope_project(
    pool: &sqlx::PgPool,
    scope: &str,
    project: Option<&str>,
) -> Result<Option<i32>, (StatusCode, String)> {
    let project = project.map(str::trim).filter(|s| !s.is_empty());
    match scope {
        "project" => {
            let name = project.ok_or((
                StatusCode::BAD_REQUEST,
                "project is required when scope='project'".to_string(),
            ))?;
            let id: Option<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
                .bind(name)
                .fetch_optional(pool)
                .await
                .map_err(map_db_500)?;
            id.map(Some)
                .ok_or((StatusCode::BAD_REQUEST, format!("unknown project '{name}'")))
        }
        _ => match project {
            Some(_) => Err((
                StatusCode::BAD_REQUEST,
                format!("project is only valid when scope='project' (got scope='{scope}')"),
            )),
            None => Ok(None),
        },
    }
}

/// Trim to a non-empty owned string, else `None` (COALESCE "leave unchanged").
fn nonblank(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

// ============================================================================
// POST /api/mandates/durable — create an operator-authored durable mandate
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct CreateDurableMandateRequest {
    pub scope: String,
    #[serde(default)]
    pub project: Option<String>,
    pub polarity: String,
    pub imperative: String,
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DurableMandateResponse {
    pub mandate: DurableMandateRow,
}

pub(crate) async fn create_durable_mandate(
    State(state): State<ApiState>,
    peer: OptionalPeer,
    Json(req): Json<CreateDurableMandateRequest>,
) -> Result<Json<DurableMandateResponse>, (StatusCode, String)> {
    writes_enabled_or_403(&state)?;
    let pool = operator_pool(&state)?;

    let scope = validate_mandate_scope(&req.scope)?;
    let polarity = validate_polarity(&req.polarity)?;
    let imperative = req.imperative.trim();
    if imperative.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "imperative must be non-empty".to_string(),
        ));
    }
    let project_id = resolve_scope_project(pool, &scope, req.project.as_deref()).await?;
    let target = nonblank(req.target.as_deref());
    let ip = request_ip(peer);

    let mut tx = pool.begin().await.map_err(map_db_500)?;
    let row = create_durable_mandate_in_tx(
        &mut tx,
        &scope,
        project_id,
        &polarity,
        imperative,
        target.as_deref(),
        OPERATOR,
    )
    .await
    .map_err(map_db_500)?;

    emit_in_tx(
        &mut tx,
        &RealtimeEvent::mandate_upsert(
            row.id,
            &row.scope,
            &row.polarity,
            &row.imperative,
            row.target.as_deref(),
        ),
    )
    .await
    .map_err(map_db_500)?;

    audit_write_tx(
        &mut tx,
        &AuditEntry {
            actor: OPERATOR.to_string(),
            action: AuditAction::MandateCreate,
            target_kind: Some("durable_mandate".to_string()),
            target_id: Some(row.id.to_string()),
            request_ip: ip,
            before: None,
            after: serde_json::to_value(&row).ok(),
            reason: None,
            ok: true,
            error: None,
        },
    )
    .await
    .map_err(map_db_500)?;

    tx.commit().await.map_err(map_db_500)?;
    Ok(Json(DurableMandateResponse { mandate: row }))
}

// ============================================================================
// PATCH /api/mandates/durable/{id} — edit an operator durable mandate
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct UpdateDurableMandateRequest {
    #[serde(default)]
    pub imperative: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub polarity: Option<String>,
    /// Editor-save alias for `imperative` — the console's inline editor POSTs
    /// `{text}`; when `imperative` is absent, `text` supplies the new rule body.
    #[serde(default)]
    pub text: Option<String>,
}

pub(crate) async fn update_durable_mandate(
    State(state): State<ApiState>,
    peer: OptionalPeer,
    Path(id): Path<i64>,
    Json(req): Json<UpdateDurableMandateRequest>,
) -> Result<Json<DurableMandateResponse>, (StatusCode, String)> {
    writes_enabled_or_403(&state)?;
    let pool = operator_pool(&state)?;

    // `text` is the editor-save alias for `imperative` (explicit `imperative`
    // wins when both are present).
    let imperative = nonblank(req.imperative.as_deref().or(req.text.as_deref()));
    let target = nonblank(req.target.as_deref());
    let polarity = match req
        .polarity
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(p) => Some(validate_polarity(p)?),
        None => None,
    };
    if imperative.is_none() && target.is_none() && polarity.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            "no fields to update: supply at least one of imperative (or text), target, polarity"
                .to_string(),
        ));
    }
    let ip = request_ip(peer);

    let mut tx = pool.begin().await.map_err(map_db_500)?;

    // Capture + lock the pre-image for the audit `before`.
    let before = get_durable_mandate_for_update_in_tx(&mut tx, id)
        .await
        .map_err(map_db_500)?
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("no durable mandate with id {id}"),
        ))?;
    if before.retired_at.is_some() {
        return Err((
            StatusCode::CONFLICT,
            format!("durable mandate {id} is retired and cannot be edited"),
        ));
    }

    let after = update_durable_mandate_in_tx(
        &mut tx,
        id,
        imperative.as_deref(),
        target.as_deref(),
        polarity.as_deref(),
    )
    .await
    .map_err(map_db_500)?
    .ok_or((
        StatusCode::NOT_FOUND,
        format!("no live durable mandate with id {id}"),
    ))?;

    emit_in_tx(
        &mut tx,
        &RealtimeEvent::mandate_upsert(
            after.id,
            &after.scope,
            &after.polarity,
            &after.imperative,
            after.target.as_deref(),
        ),
    )
    .await
    .map_err(map_db_500)?;

    audit_write_tx(
        &mut tx,
        &AuditEntry {
            actor: OPERATOR.to_string(),
            action: AuditAction::MandateUpdate,
            target_kind: Some("durable_mandate".to_string()),
            target_id: Some(id.to_string()),
            request_ip: ip,
            before: serde_json::to_value(&before).ok(),
            after: serde_json::to_value(&after).ok(),
            reason: None,
            ok: true,
            error: None,
        },
    )
    .await
    .map_err(map_db_500)?;

    tx.commit().await.map_err(map_db_500)?;
    Ok(Json(DurableMandateResponse { mandate: after }))
}

// ============================================================================
// POST /api/mandates/durable/{id}/retire — soft-delete a durable mandate
// ============================================================================

#[derive(Debug, Default, Deserialize)]
pub struct RetireDurableMandateRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

pub(crate) async fn retire_durable_mandate(
    State(state): State<ApiState>,
    peer: OptionalPeer,
    Path(id): Path<i64>,
    body: Option<Json<RetireDurableMandateRequest>>,
) -> Result<Json<DurableMandateResponse>, (StatusCode, String)> {
    writes_enabled_or_403(&state)?;
    let pool = operator_pool(&state)?;
    let reason = body.and_then(|b| nonblank(b.0.reason.as_deref()));
    let ip = request_ip(peer);

    let mut tx = pool.begin().await.map_err(map_db_500)?;

    let before = get_durable_mandate_for_update_in_tx(&mut tx, id)
        .await
        .map_err(map_db_500)?
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("no durable mandate with id {id}"),
        ))?;
    if before.retired_at.is_some() {
        return Err((
            StatusCode::CONFLICT,
            format!("durable mandate {id} is already retired"),
        ));
    }

    let after = retire_durable_mandate_in_tx(&mut tx, id)
        .await
        .map_err(map_db_500)?
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("no live durable mandate with id {id}"),
        ))?;

    // A retirement is a delete from the operator's point of view — emit the
    // `mandate` delete op so the realtime feed drops it from the active list.
    emit_in_tx(
        &mut tx,
        &RealtimeEvent::mandate_delete(after.id, &after.polarity, &after.imperative),
    )
    .await
    .map_err(map_db_500)?;

    audit_write_tx(
        &mut tx,
        &AuditEntry {
            actor: OPERATOR.to_string(),
            action: AuditAction::MandateRetire,
            target_kind: Some("durable_mandate".to_string()),
            target_id: Some(id.to_string()),
            request_ip: ip,
            before: serde_json::to_value(&before).ok(),
            after: serde_json::to_value(&after).ok(),
            reason,
            ok: true,
            error: None,
        },
    )
    .await
    .map_err(map_db_500)?;

    tx.commit().await.map_err(map_db_500)?;
    Ok(Json(DurableMandateResponse { mandate: after }))
}

// ============================================================================
// POST /api/mandates/promote — promote a session mandate to durable scope
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct PromoteMandateRequest {
    pub session_mandate_id: i64,
    pub scope: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub write_to_file: Option<bool>,
    #[serde(default)]
    pub target_file: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PromoteMandateResponse {
    pub durable_mandate_id: i64,
    pub source_session_mandate_id: i64,
    pub scope: String,
    pub project_id: Option<i32>,
    /// The file the promotion bullet was appended to, when `write_to_file` was
    /// requested and the append succeeded; `null` otherwise (the DB promotion is
    /// authoritative — a file-mirror failure is logged, not fatal).
    pub wrote_file: Option<String>,
}

pub(crate) async fn promote_mandate(
    State(state): State<ApiState>,
    peer: OptionalPeer,
    Json(req): Json<PromoteMandateRequest>,
) -> Result<Json<PromoteMandateResponse>, (StatusCode, String)> {
    writes_enabled_or_403(&state)?;
    let pool = operator_pool(&state)?;

    if req.session_mandate_id <= 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            "session_mandate_id must be positive".to_string(),
        ));
    }
    let scope = validate_mandate_scope(&req.scope)?;
    let project_id = resolve_scope_project(pool, &scope, req.project.as_deref()).await?;
    let write_to_file = req.write_to_file.unwrap_or(false);
    let target_file = nonblank(req.target_file.as_deref());
    if write_to_file && target_file.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            "write_to_file=true requires a non-empty target_file (no implicit \
             CLAUDE.md/AGENTS.md path is chosen for safety)"
                .to_string(),
        ));
    }
    let ip = request_ip(peer);

    let mut tx = pool.begin().await.map_err(map_db_500)?;

    // Reuse the session-mandate promotion logic in-tx (it emits the `mandate`
    // upsert realtime event itself, bound to this tx) and add only the audit row
    // — so promotion + event + audit commit atomically.
    let durable_id = crate::sessions::promote_mandate_in_tx(
        &mut tx,
        req.session_mandate_id,
        &scope,
        project_id,
        target_file.as_deref(),
    )
    .await
    .map_err(promote_err_to_http)?;

    audit_write_tx(
        &mut tx,
        &AuditEntry {
            actor: OPERATOR.to_string(),
            action: AuditAction::MandatePromote,
            target_kind: Some("durable_mandate".to_string()),
            target_id: Some(durable_id.to_string()),
            request_ip: ip,
            before: None,
            after: Some(serde_json::json!({
                "durable_mandate_id": durable_id,
                "source_session_mandate_id": req.session_mandate_id,
                "scope": scope.as_str(),
                "project_id": project_id,
            })),
            reason: None,
            ok: true,
            error: None,
        },
    )
    .await
    .map_err(map_db_500)?;

    tx.commit().await.map_err(map_db_500)?;

    // Optional belt-and-suspenders file mirror (post-commit; the DB promotion is
    // already durable). Best-effort: a failure is logged at error! (ADR-021 —
    // a degraded fallback) and reported as `wrote_file: null`, never rolls back
    // the committed promotion. Reuses the exact marker + bullet format as the
    // MCP `promote_session_mandate` path so both append to the same section.
    let mut wrote_file: Option<String> = None;
    if let Some(path) = target_file.as_deref() {
        match crate::sessions::get_mandate(pool, req.session_mandate_id).await {
            Ok(Some(mandate)) => {
                let bullet = format!(
                    "- **{}** _(scope: {})_: {}",
                    mandate.polarity, scope, mandate.imperative
                );
                match crate::mcp::tools::tool_session_mandates::append_bullet_to_marker(
                    path,
                    PROMOTED_MARKER,
                    &bullet,
                ) {
                    Ok(()) => wrote_file = Some(path.to_string()),
                    Err(e) => tracing::error!(
                        error = %e,
                        path,
                        "webui promote: append_bullet_to_marker failed; DB promotion stands"
                    ),
                }
            }
            Ok(None) => tracing::error!(
                mandate_id = req.session_mandate_id,
                "webui promote: source session mandate vanished before file mirror"
            ),
            Err(e) => tracing::error!(
                error = %e,
                "webui promote: get_mandate for file mirror failed; DB promotion stands"
            ),
        }
    }

    Ok(Json(PromoteMandateResponse {
        durable_mandate_id: durable_id,
        source_session_mandate_id: req.session_mandate_id,
        scope,
        project_id,
        wrote_file,
    }))
}

/// Map `promote_mandate_in_tx`'s `sqlx::Error` to an HTTP status. Mirrors the
/// MCP `promote_session_mandate` mapping: a missing/ineligible source is a 404,
/// a `Protocol` conflict (already promoted with a different scope/target, or the
/// source is not active) is a 409, and anything else is a 500.
fn promote_err_to_http(e: sqlx::Error) -> (StatusCode, String) {
    match e {
        sqlx::Error::RowNotFound => (
            StatusCode::NOT_FOUND,
            "session mandate not found or not eligible for promotion".to_string(),
        ),
        sqlx::Error::Protocol(msg) => (StatusCode::CONFLICT, msg),
        other => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("promote failed: {other}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_vocabulary_is_enforced() {
        assert_eq!(validate_mandate_scope(" Global ").unwrap(), "global");
        assert_eq!(validate_mandate_scope("project").unwrap(), "project");
        assert_eq!(validate_mandate_scope("workspace").unwrap(), "workspace");
        // session is NOT a durable scope; must be rejected as a client error.
        assert_eq!(
            validate_mandate_scope("session").unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            validate_mandate_scope("bogus").unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn polarity_vocabulary_is_enforced() {
        assert_eq!(validate_polarity("never").unwrap(), "never");
        assert_eq!(validate_polarity(" process_rule ").unwrap(), "process_rule");
        assert_eq!(
            validate_polarity("sometimes").unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn promote_conflict_maps_to_409_and_missing_to_404() {
        assert_eq!(
            promote_err_to_http(sqlx::Error::RowNotFound).0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            promote_err_to_http(sqlx::Error::Protocol("already promoted".to_string())).0,
            StatusCode::CONFLICT
        );
    }

    #[test]
    fn emitted_audit_actions_are_valid() {
        // Golden: every AuditAction this module writes is in the closed,
        // CHECK-pinned vocabulary (so the v66 `webui_audit_log_action_check`
        // constraint can never reject a row we emit).
        for a in [
            AuditAction::MandateCreate,
            AuditAction::MandateUpdate,
            AuditAction::MandateRetire,
            AuditAction::MandatePromote,
        ] {
            assert!(
                AuditAction::ALL.contains(&a),
                "{} missing from AuditAction::ALL",
                a.as_str()
            );
        }
    }
}
