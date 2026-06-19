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
    WorkItemAddCriterionParams, WorkItemAssertFixedParams, WorkItemAttemptVerifyParams,
    WorkItemDeferParams, WorkItemRecordEvidenceParams, WorkItemReinstateParams,
};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::work_items::crud::{id_of_public, map_db_err, map_op_err};
use crate::tracker::status::WorkItemStatus;
use crate::tracker::transition::Actor;

const MAX_CRITERION_DESCRIPTION_BYTES: usize = 4096;
const MAX_ACCEPTANCE_URI_BYTES: usize = 2048;
const MAX_EVIDENCE_DETAIL_JSON_BYTES: usize = 16 * 1024;
const MAX_COMMIT_SHA_BYTES: usize = 128;

fn valid_criterion_kind(value: &str) -> bool {
    matches!(
        value,
        "test"
            | "build"
            | "lint"
            | "proof"
            | "model_check"
            | "smt"
            | "script"
            | "auditor_verdict"
            | "manual_user_signoff"
            | "experiment_verdict"
    )
}

fn valid_coverage_mode(value: &str) -> bool {
    matches!(value, "single" | "universal")
}

fn valid_gate(value: &str) -> bool {
    matches!(
        value,
        "alpha_antistub" | "beta_verify" | "gamma_audit" | "formal"
    )
}

/// `work_item_add_criterion` — attach a machine-checkable acceptance criterion
/// to an item. The MCP boundary validates the closed vocabularies before the
/// DB CHECKs, so bad values surface as `invalid_params` without a failed write.
pub async fn tool_work_item_add_criterion(
    ctx: &SystemContext,
    params: WorkItemAddCriterionParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .work_item_queries
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let public_id = params.public_id.trim();
    let criterion_kind = params.criterion_kind.trim();
    if !valid_criterion_kind(criterion_kind) {
        return Err(McpError::invalid_params(
            "criterion_kind must be one of test|build|lint|proof|model_check|smt|script|auditor_verdict|manual_user_signoff|experiment_verdict",
            None,
        ));
    }
    let description = params.description.trim();
    if description.is_empty() {
        return Err(McpError::invalid_params(
            "description must be non-empty",
            None,
        ));
    }
    if description.len() > MAX_CRITERION_DESCRIPTION_BYTES {
        return Err(McpError::invalid_params(
            format!("description must be at most {MAX_CRITERION_DESCRIPTION_BYTES} bytes"),
            None,
        ));
    }
    let acceptance_uri = params
        .acceptance_uri
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(uri) = acceptance_uri
        && uri.len() > MAX_ACCEPTANCE_URI_BYTES
    {
        return Err(McpError::invalid_params(
            format!("acceptance_uri must be at most {MAX_ACCEPTANCE_URI_BYTES} bytes"),
            None,
        ));
    }
    let coverage_mode = params
        .coverage_mode
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("single");
    if !valid_coverage_mode(coverage_mode) {
        return Err(McpError::invalid_params(
            "coverage_mode must be one of single|universal",
            None,
        ));
    }
    let gate = params
        .gate
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(gate) = gate
        && !valid_gate(gate)
    {
        return Err(McpError::invalid_params(
            "gate must be one of alpha_antistub|beta_verify|gamma_audit|formal",
            None,
        ));
    }
    let item_id = id_of_public(pool, public_id).await?;
    let required = params.required.unwrap_or(true);
    let cid = queries::insert_acceptance_criterion(
        pool,
        item_id,
        criterion_kind,
        description,
        acceptance_uri,
        params.expect_exit,
        coverage_mode,
        gate,
        required,
    )
    .await
    .map_err(|e| McpError::invalid_params(format!("criterion rejected: {e}"), None))?;
    json_result(&json!({
        "criterion_id": cid,
        "item": public_id,
        "criterion_kind": criterion_kind,
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
    let pool = pool_or_err(ctx)?;
    if params.criterion_id <= 0 {
        return Err(McpError::invalid_params(
            "criterion_id must be positive",
            None,
        ));
    }
    let verdict = params.verdict.trim();
    if !matches!(verdict, "pass" | "fail" | "unknown" | "error") {
        return Err(McpError::invalid_params(
            "verdict must be one of pass|fail|unknown|error",
            None,
        ));
    }
    if let Some(count) = params.coverage_count
        && count < 0
    {
        return Err(McpError::invalid_params(
            "coverage_count must be non-negative",
            None,
        ));
    }
    if let Some(total) = params.coverage_total
        && total < 0
    {
        return Err(McpError::invalid_params(
            "coverage_total must be non-negative",
            None,
        ));
    }
    if let (Some(count), Some(total)) = (params.coverage_count, params.coverage_total)
        && count > total
    {
        return Err(McpError::invalid_params(
            "coverage_count must not exceed coverage_total",
            None,
        ));
    }
    let commit_sha = params
        .commit_sha
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(commit_sha) = commit_sha
        && commit_sha.len() > MAX_COMMIT_SHA_BYTES
    {
        return Err(McpError::invalid_params(
            format!("commit_sha must be at most {MAX_COMMIT_SHA_BYTES} bytes"),
            None,
        ));
    }
    let detail = params
        .detail_json
        .clone()
        .unwrap_or_else(|| "{}".to_string());
    if detail.len() > MAX_EVIDENCE_DETAIL_JSON_BYTES {
        return Err(McpError::invalid_params(
            format!("detail_json must be at most {MAX_EVIDENCE_DETAIL_JSON_BYTES} bytes"),
            None,
        ));
    }
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
        commit_sha,
        None,
        &detail,
    )
    .await
    .map_err(|e| {
        McpError::invalid_params(format!("evidence rejected (unknown criterion?): {e}"), None)
    })?;
    ctx.stats()
        .work_item_evidence_recorded
        .fetch_add(1, Ordering::Relaxed);
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
    ctx.stats()
        .work_item_verifications
        .fetch_add(1, Ordering::Relaxed);
    json_result(&updated)
}

/// `work_item_assert_fixed` — an agent asserts a bug is fixed against a frozen,
/// machine-checkable reproduction criterion, and advances it as far as the
/// trust boundary allows (`claimed_done`). It does NOT — and cannot — mark the
/// bug `verified`: per ADR-004 an `Actor::Agent` has no arm into a judgment
/// state. Closing to `verified` happens only via trusted evidence — CI posting
/// the `verification_command`'s result (`POST /api/tracker/ci_evidence` with the
/// tracker user_token) or a decided bug-fix experiment — at which point the
/// gatekeeper flips it. This tool freezes the bar (anti-tamper) and walks the
/// agent-legal part of the path, then reports exactly what trusted step remains.
/// It is the self-service "I fixed it" affordance whose absence left agent-filed
/// bugs stranded (ADR-023).
pub async fn tool_work_item_assert_fixed(
    ctx: &SystemContext,
    params: WorkItemAssertFixedParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    if params.verification_command.trim().is_empty() {
        return Err(McpError::invalid_params(
            "verification_command must be non-empty (a check that fails before the fix and passes after)",
            None,
        ));
    }
    let item_id = id_of_public(pool, &params.public_id).await?;
    let item = queries::get_work_item(pool, item_id)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| {
            McpError::invalid_params(format!("no work item '{}'", params.public_id), None)
        })?;
    if item.kind != "bug" {
        return Err(McpError::invalid_params(
            format!(
                "work_item_assert_fixed applies only to kind='bug' (got '{}')",
                item.kind
            ),
            None,
        ));
    }

    // Freeze the verifiable criterion (anti-tamper; locked once).
    queries::freeze_bug_criterion(
        pool,
        item_id,
        &params.verification_command,
        params.expected_signal.as_deref(),
    )
    .await
    .map_err(map_db_err)?;

    // Walk the agent-legal part of the path to `claimed_done`. NEVER touch a
    // judgment state — that is the gatekeeper's, gated on trusted evidence.
    // Best-effort: if the from-state is not agent-advanceable to claimed_done,
    // report the current status + the transition note rather than failing (the
    // frozen criterion is the load-bearing outcome).
    let agent = params.agent_id.as_deref().unwrap_or("unknown-agent");
    let (advanced, status, transition_note) = match queries::set_work_item_status(
        pool,
        item_id,
        WorkItemStatus::ClaimedDone,
        Actor::Agent,
        Some(agent),
        Some("assert_fixed"),
        None,
        None,
    )
    .await
    {
        Ok(row) => (true, row.status, None),
        Err(e) => (false, item.status.clone(), Some(e.to_string())),
    };

    json_result(&json!({
        "public_id": params.public_id,
        "kind": "bug",
        "status": status,
        "advanced_to_claimed_done": advanced,
        "transition_note": transition_note,
        "criterion_frozen": true,
        "verification_command": params.verification_command,
        "verified": false,
        "guidance": "Fix asserted with a frozen, machine-checkable criterion. This does NOT mark \
    the bug verified — an agent cannot self-verify (ADR-004 trust boundary). To close it, post TRUSTED \
    evidence: run verification_command in CI and POST /api/tracker/ci_evidence with the tracker \
    user_token, or decide a linked bug-fix experiment; the gatekeeper then flips it to verified.",
    }))
}

/// Check the configured tracker user-token. This is the user-authority gate for
/// every user-only operation (defer / reinstate / triage-confirm / resolve): the
/// user passes the token (from their local config); the agent does not have it,
/// so an agent cannot perform these. Shared with `bugs::tool_work_item_triage`
/// and `bugs::tool_work_item_resolve`.
pub(crate) fn check_user_token(ctx: &SystemContext, provided: &str) -> Result<(), McpError> {
    let cfg = ctx.config().load();
    match cfg.tracker.user_token.as_deref() {
        None => Err(McpError::invalid_params(
            "this user-authority operation is disabled: set [tracker] user_token in config and \
             pass it as user_token (agents do not have it, so they cannot self-defer / confirm / \
             resolve)",
            None,
        )),
        Some(tok) if tok == provided => Ok(()),
        Some(_) => Err(McpError::invalid_params(
            "invalid user_token: this is a user-authority operation (agents cannot self-defer / \
             scope-cut / confirm / resolve)",
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
    check_user_token(ctx, &params.user_token)?;
    let pool = pool_or_err(ctx)?;
    let reason = params.reason.trim();
    if reason.is_empty() {
        return Err(McpError::invalid_params("reason must be non-empty", None));
    }
    let item_id = id_of_public(pool, &params.public_id).await?;
    let granted_by = params.granted_by.as_deref().unwrap_or("user");
    let mut tx = pool.begin().await.map_err(map_db_err)?;
    let neg_id =
        queries::record_scope_negotiation_in_tx(&mut tx, item_id, "defer", granted_by, reason)
            .await
            .map_err(map_db_err)?;
    let updated = queries::set_work_item_status_in_tx(
        &mut tx,
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
    tx.commit().await.map_err(map_db_err)?;
    ctx.stats()
        .work_item_status_changes
        .fetch_add(1, Ordering::Relaxed);
    json_result(&updated)
}

/// `work_item_reinstate` — USER-only: undo a deferral (deferred → in_progress).
/// Same token gate + scope-negotiation audit.
pub async fn tool_work_item_reinstate(
    ctx: &SystemContext,
    params: WorkItemReinstateParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    check_user_token(ctx, &params.user_token)?;
    let pool = pool_or_err(ctx)?;
    let reason = params.reason.trim();
    if reason.is_empty() {
        return Err(McpError::invalid_params("reason must be non-empty", None));
    }
    let item_id = id_of_public(pool, &params.public_id).await?;
    let granted_by = params.granted_by.as_deref().unwrap_or("user");
    let mut tx = pool.begin().await.map_err(map_db_err)?;
    let neg_id =
        queries::record_scope_negotiation_in_tx(&mut tx, item_id, "reinstate", granted_by, reason)
            .await
            .map_err(map_db_err)?;
    let updated = queries::set_work_item_status_in_tx(
        &mut tx,
        item_id,
        WorkItemStatus::InProgress,
        Actor::User,
        Some(granted_by),
        Some(reason),
        None,
        Some(neg_id),
    )
    .await
    .map_err(map_op_err)?;
    tx.commit().await.map_err(map_db_err)?;
    ctx.stats()
        .work_item_status_changes
        .fetch_add(1, Ordering::Relaxed);
    json_result(&updated)
}
