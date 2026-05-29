//! Bug-tracking tool bodies: `work_item_triage` (the user-token-gated
//! triage → confirmed acceptance) and `work_item_resolve` (the user-token-gated
//! close-without-fix → cancelled with a categorized resolution).
//!
//! TRUST BOUNDARY: both run as [`Actor::User`] only AFTER
//! [`super::verify::check_user_token`] succeeds — exactly like `defer` /
//! `reinstate`. The transition matrix has no `Agent` arm into `confirmed`, and
//! `→cancelled` is user-only, so an agent (which has neither the token nor a
//! `User` actor on the generic `set_status` path) can neither confirm a bug as
//! real nor close one as won't-fix. The evidence-backed `→verified` "fixed" path
//! (`attempt_verify`) is untouched.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::db::queries::{
    self, BugDetailFields, fetch_bug_details, get_work_item_by_public_id, insert_relation,
    record_scope_negotiation, set_work_item_status, update_work_item_fields,
};
use crate::mcp::server::{WorkItemResolveParams, WorkItemTriageParams};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::work_items::crud::{id_of_public, map_db_err, map_op_err};
use crate::mcp::tools::work_items::nonblank;
use crate::mcp::tools::work_items::verify::check_user_token;
use crate::tracker::severity::{BugResolution, Severity};
use crate::tracker::status::WorkItemStatus;
use crate::tracker::transition::Actor;

// ============================================================================
// work_item_triage  (USER-only: triage → confirmed)
// ============================================================================

pub async fn tool_work_item_triage(
    ctx: &SystemContext,
    params: WorkItemTriageParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .work_item_status_changes
        .fetch_add(1, Ordering::Relaxed);
    check_user_token(ctx, &params.user_token)?;
    let pool = pool_or_err(ctx)?;

    let item = get_work_item_by_public_id(pool, &params.public_id)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| {
            McpError::invalid_params(format!("no work item '{}'", params.public_id), None)
        })?;
    if item.kind != "bug" {
        return Err(McpError::invalid_params(
            "work_item_triage applies to bugs (kind='bug'); use work_item_set_status for other kinds",
            None,
        ));
    }

    // Resolve the effective severity (a new param wins; else the stored value);
    // a bug cannot be confirmed without one.
    let parsed_sev = match nonblank(&params.severity) {
        Some(s) => Some(Severity::parse(s).ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "unknown severity '{s}'; expected one of {}",
                    crate::tracker::severity::sql_in_list()
                ),
                None,
            )
        })?),
        None => None,
    };
    let have_severity = parsed_sev.is_some() || item.severity.is_some();
    if !have_severity {
        return Err(McpError::invalid_params(
            "a severity is required to confirm a bug — pass severity=critical|high|medium|low \
             (or set it first via work_item_update)",
            None,
        ));
    }

    // Reproduction must be present (a new param or an already-recorded one).
    let existing = fetch_bug_details(pool, item.id).await.map_err(map_db_err)?;
    let new_repro = nonblank(&params.reproduction_steps);
    let have_repro = new_repro.is_some()
        || existing
            .as_ref()
            .and_then(|d| d.reproduction_steps.as_deref())
            .is_some_and(|s| !s.trim().is_empty());
    if !have_repro {
        return Err(McpError::invalid_params(
            "reproduction_steps are required to confirm a bug — pass reproduction_steps=…",
            None,
        ));
    }

    // Set severity on the spine if a new one was supplied. Derive a default
    // priority ONLY when the item still carries the default priority 0 — never
    // clobber an explicit one.
    if let Some(sev) = parsed_sev {
        let derived_priority = (item.priority == 0).then(|| sev.default_priority());
        update_work_item_fields(
            pool,
            item.id,
            None,
            None,
            derived_priority,
            None,
            None,
            false,
            None,
            false,
            Some(sev.as_str()),
        )
        .await
        .map_err(map_op_err)?;
    }

    // Record the triage milestone + any new reproduction / root-cause.
    let triaged_by = params.triaged_by.as_deref().unwrap_or("user");
    let fields = BugDetailFields {
        reproduction_steps: new_repro,
        root_cause: nonblank(&params.root_cause),
        triaged_by: Some(triaged_by),
        set_triaged_at: true,
        ..Default::default()
    };
    queries::upsert_bug_details(pool, item.id, &fields)
        .await
        .map_err(map_db_err)?;

    // The user-authority transition triage → confirmed (Actor::User; the token
    // was checked above, mirroring defer). The matrix refuses this for any
    // non-user actor, so an agent on the generic set_status path cannot reach it.
    let updated = set_work_item_status(
        pool,
        item.id,
        WorkItemStatus::Confirmed,
        Actor::User,
        Some(triaged_by),
        Some("triage: confirmed"),
        None,
        None,
    )
    .await
    .map_err(map_op_err)?;

    let details = fetch_bug_details(pool, item.id).await.map_err(map_db_err)?;
    json_result(&json!({ "item": updated, "bug_details": details }))
}

// ============================================================================
// work_item_resolve  (USER-only: close without a fix → cancelled)
// ============================================================================

pub async fn tool_work_item_resolve(
    ctx: &SystemContext,
    params: WorkItemResolveParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .work_item_status_changes
        .fetch_add(1, Ordering::Relaxed);
    check_user_token(ctx, &params.user_token)?;
    let pool = pool_or_err(ctx)?;

    let resolution = params.resolution.trim();
    let res = BugResolution::parse(resolution).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "unknown resolution '{resolution}'; expected one of {}",
                crate::tracker::severity::resolution_sql_in_list()
            ),
            None,
        )
    })?;
    if !res.is_user_settable() {
        return Err(McpError::invalid_params(
            "'fixed' is reached via the evidence-backed verify path (work_item_attempt_verify), \
             not work_item_resolve; pass a non-fixed resolution to close without a fix",
            None,
        ));
    }
    let reason = params.reason.trim();
    if reason.is_empty() {
        return Err(McpError::invalid_params("reason must be non-empty", None));
    }

    let item = get_work_item_by_public_id(pool, &params.public_id)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| {
            McpError::invalid_params(format!("no work item '{}'", params.public_id), None)
        })?;
    if item.kind != "bug" {
        return Err(McpError::invalid_params(
            "work_item_resolve applies to bugs (kind='bug'); use work_item_set_status / \
             work_item_defer for other kinds",
            None,
        ));
    }
    let item_id = item.id;
    let granted_by = params.granted_by.as_deref().unwrap_or("user");

    // resolution=duplicate may record the canonical 'duplicates' relation.
    if let Some(dup) = nonblank(&params.duplicate_of) {
        let target_id = id_of_public(pool, dup).await?;
        if target_id == item_id {
            return Err(McpError::invalid_params(
                "an item cannot be a duplicate of itself",
                None,
            ));
        }
        insert_relation(pool, item_id, target_id, "duplicates", Some(granted_by))
            .await
            .map_err(map_db_err)?;
    }

    // Persist the categorized resolution (+ optional fixed-in version / root
    // cause) on the bug-detail sidecar.
    let fields = BugDetailFields {
        resolution: Some(res.as_str()),
        fixed_in_version: nonblank(&params.fixed_in_version),
        root_cause: nonblank(&params.root_cause),
        ..Default::default()
    };
    queries::upsert_bug_details(pool, item_id, &fields)
        .await
        .map_err(map_db_err)?;

    // Close → cancelled (Actor::User), recording an append-only scope
    // negotiation for the audit trail (action='cancel').
    let neg_id = record_scope_negotiation(pool, item_id, "cancel", granted_by, reason)
        .await
        .map_err(map_db_err)?;
    let updated = set_work_item_status(
        pool,
        item_id,
        WorkItemStatus::Cancelled,
        Actor::User,
        Some(granted_by),
        Some(reason),
        None,
        Some(neg_id),
    )
    .await
    .map_err(map_op_err)?;

    let details = fetch_bug_details(pool, item_id).await.map_err(map_db_err)?;
    json_result(&json!({ "item": updated, "bug_details": details, "resolution": res.as_str() }))
}
