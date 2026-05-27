//! Verification / gatekeeping tools: define machine-checkable acceptance
//! criteria, record evidence, and attempt the gatekeeper `→verified`
//! transition.
//!
//! TRUST BOUNDARY: the MCP `work_item_record_evidence` tool FORCES
//! `source='manual'` — an agent **cannot** post trusted-source evidence.
//! Trusted evidence (`ci`/`stop_hook`/`subagent_audit`/`external_auditor`/
//! `user_signoff`/`experiment`) arrives only via the credential-gated REST
//! endpoint (hooks/CI, Phase 6) or the experiment engine (Phase 10). And
//! `work_item_attempt_verify` runs the transition as `Actor::Gatekeeper`, which
//! only succeeds when `set_work_item_status`'s evidence gate finds passing,
//! trusted evidence for every required criterion. So an agent can define
//! criteria and record manual notes, but can never make its own work
//! `verified`.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::{
    WorkItemAddCriterionParams, WorkItemAttemptVerifyParams, WorkItemDeferParams,
    WorkItemRecordEvidenceParams, WorkItemReinstateParams,
};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::work_items::crud::{id_of_public, map_db_err, map_op_err};
use crate::tracker::status::WorkItemStatus;
use crate::tracker::transition::Actor;

/// `work_item_add_criterion` — attach a machine-checkable acceptance criterion
/// to an item. The DB CHECKs validate `criterion_kind`/`coverage_mode`/`gate`;
/// a bad value surfaces as `invalid_params`.
pub async fn tool_work_item_add_criterion(
    ctx: &SystemContext,
    params: WorkItemAddCriterionParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .work_item_queries
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let item_id = id_of_public(pool, &params.public_id).await?;
    let description = params.description.trim();
    if description.is_empty() {
        return Err(McpError::invalid_params(
            "description must be non-empty",
            None,
        ));
    }
    let coverage_mode = params.coverage_mode.as_deref().unwrap_or("single");
    let required = params.required.unwrap_or(true);
    let cid = queries::insert_acceptance_criterion(
        pool,
        item_id,
        params.criterion_kind.trim(),
        description,
        params.acceptance_uri.as_deref(),
        params.expect_exit,
        coverage_mode,
        params.gate.as_deref(),
        required,
    )
    .await
    .map_err(|e| McpError::invalid_params(format!("criterion rejected: {e}"), None))?;
    json_result(&json!({
        "criterion_id": cid,
        "item": params.public_id,
        "criterion_kind": params.criterion_kind.trim(),
        "coverage_mode": coverage_mode,
        "required": required,
    }))
}

/// `work_item_record_evidence` — record evidence for a criterion. On the MCP
/// path the `source` is forced to `'manual'`, which does NOT satisfy the
/// verified gate (see the module-level trust note).
pub async fn tool_work_item_record_evidence(
    ctx: &SystemContext,
    params: WorkItemRecordEvidenceParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .work_item_evidence_recorded
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let verdict = params.verdict.trim();
    if !matches!(verdict, "pass" | "fail" | "unknown" | "error") {
        return Err(McpError::invalid_params(
            "verdict must be one of pass|fail|unknown|error",
            None,
        ));
    }
    let detail = params
        .detail_json
        .clone()
        .unwrap_or_else(|| "{}".to_string());
    // Validate detail_json is well-formed JSON before binding it as ::jsonb.
    if serde_json::from_str::<Value>(&detail).is_err() {
        return Err(McpError::invalid_params(
            "detail_json must be valid JSON",
            None,
        ));
    }
    let eid = queries::record_verification_evidence(
        pool,
        params.criterion_id,
        verdict,
        "manual", // TRUST: MCP callers cannot supply a trusted source.
        params.exit_code,
        params.coverage_count,
        params.coverage_total,
        None,
        None,
        params.commit_sha.as_deref(),
        None,
        &detail,
    )
    .await
    .map_err(|e| {
        McpError::invalid_params(format!("evidence rejected (unknown criterion?): {e}"), None)
    })?;
    json_result(&json!({
        "evidence_id": eid,
        "criterion_id": params.criterion_id,
        "verdict": verdict,
        "source": "manual",
        "note": "MCP-recorded evidence is source='manual' and does NOT satisfy the verified gate; \
                 trusted evidence (ci/stop_hook/external_auditor/experiment) arrives via the \
                 credential-gated REST endpoint or the experiment engine.",
    }))
}

/// `work_item_attempt_verify` — try the gatekeeper `→verified` transition. It
/// succeeds only if every required criterion has passing trusted-source
/// evidence (enforced by `set_work_item_status`'s gate); otherwise it returns
/// the explanatory transition error (e.g. "verified is reached only by
/// submitting passing acceptance evidence"). The item must first be in
/// `claimed_done` or `verifying`.
pub async fn tool_work_item_attempt_verify(
    ctx: &SystemContext,
    params: WorkItemAttemptVerifyParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .work_item_verifications
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let item_id = id_of_public(pool, &params.public_id).await?;
    let evidence_id = queries::latest_passing_evidence_id(pool, item_id)
        .await
        .map_err(map_db_err)?;
    let updated = queries::set_work_item_status(
        pool,
        item_id,
        WorkItemStatus::Verified,
        Actor::Gatekeeper,
        Some("gatekeeper"),
        Some("attempt_verify"),
        evidence_id,
        None,
    )
    .await
    .map_err(map_op_err)?;
    json_result(&updated)
}

/// Check the configured tracker user-token. This is the user-authority gate for
/// defer/reinstate: the user passes the token (from their local config); the
/// agent does not have it, so an agent cannot self-defer.
fn check_user_token(ctx: &SystemContext, provided: &str) -> Result<(), McpError> {
    let cfg = ctx.config().load();
    match cfg.tracker.user_token.as_deref() {
        None => Err(McpError::invalid_params(
            "defer/reinstate is disabled: set [tracker] user_token in config and pass it as \
             user_token (this is a user-authority op — agents cannot self-defer)",
            None,
        )),
        Some(tok) if tok == provided => Ok(()),
        Some(_) => Err(McpError::invalid_params(
            "invalid user_token: defer/reinstate is a user-authority operation (agents cannot \
             self-defer / scope-cut)",
            None,
        )),
    }
}

/// `work_item_defer` — USER-only: explicitly skip an item (excluded from
/// completion roll-up). Requires the tracker user-token and records an
/// append-only `scope_negotiations` row. Agents cannot reach this (no token,
/// and `→deferred` has no agent arm in the transition matrix).
pub async fn tool_work_item_defer(
    ctx: &SystemContext,
    params: WorkItemDeferParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .work_item_status_changes
        .fetch_add(1, Ordering::Relaxed);
    check_user_token(ctx, &params.user_token)?;
    let pool = pool_or_err(ctx)?;
    let reason = params.reason.trim();
    if reason.is_empty() {
        return Err(McpError::invalid_params("reason must be non-empty", None));
    }
    let item_id = id_of_public(pool, &params.public_id).await?;
    let granted_by = params.granted_by.as_deref().unwrap_or("user");
    let neg_id = queries::record_scope_negotiation(pool, item_id, "defer", granted_by, reason)
        .await
        .map_err(map_db_err)?;
    let updated = queries::set_work_item_status(
        pool,
        item_id,
        WorkItemStatus::Deferred,
        Actor::User,
        Some(granted_by),
        Some(reason),
        None,
        Some(neg_id),
    )
    .await
    .map_err(map_op_err)?;
    json_result(&updated)
}

/// `work_item_reinstate` — USER-only: undo a deferral (deferred → in_progress).
/// Same token gate + scope-negotiation audit.
pub async fn tool_work_item_reinstate(
    ctx: &SystemContext,
    params: WorkItemReinstateParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .work_item_status_changes
        .fetch_add(1, Ordering::Relaxed);
    check_user_token(ctx, &params.user_token)?;
    let pool = pool_or_err(ctx)?;
    let reason = params.reason.trim();
    if reason.is_empty() {
        return Err(McpError::invalid_params("reason must be non-empty", None));
    }
    let item_id = id_of_public(pool, &params.public_id).await?;
    let granted_by = params.granted_by.as_deref().unwrap_or("user");
    queries::record_scope_negotiation(pool, item_id, "reinstate", granted_by, reason)
        .await
        .map_err(map_db_err)?;
    let updated = queries::set_work_item_status(
        pool,
        item_id,
        WorkItemStatus::InProgress,
        Actor::User,
        Some(granted_by),
        Some(reason),
        None,
        None,
    )
    .await
    .map_err(map_op_err)?;
    json_result(&updated)
}
