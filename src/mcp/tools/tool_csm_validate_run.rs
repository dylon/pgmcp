//! `csm_validate_run` — the keystone observer. Lift a completed `a2a_pattern_*`
//! run (by `a2a_tasks` id) into a protocol trace, check it against the pattern's
//! projected network, persist the verdict + MSM-encoded series to
//! `csm_run_traces`, and report where the run sits relative to prior conformant
//! / non-conformant runs (the Phase-3 MSM bridge).
//!
//! The read→lift→conformance core is shared with the `csm-validate` cron via
//! `csm::validate::prepare_validation`; this tool adds the MSM-trend probe and
//! the rich JSON envelope, and always persists (manual re-validation is allowed).

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::context::SystemContext;
use crate::csm::store::{insert_run_trace, load_protocol_cohorts};
use crate::csm::validate::{Prepared, prepare_validation};
use crate::fuzzy::trajectory_index::{DEFAULT_MSM_C, classify_trend, load_msm_c};
use crate::mcp::server::CsmValidateRunParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_csm_validate_run(
    ctx: &SystemContext,
    params: CsmValidateRunParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let task_id = Uuid::parse_str(&params.task_id).map_err(|e| {
        McpError::invalid_params(format!("bad task_id '{}': {e}", params.task_id), None)
    })?;

    let ready = match prepare_validation(pool, task_id)
        .await
        .map_err(|e| McpError::internal_error(e, None))?
    {
        Prepared::NotFound => {
            return Err(McpError::internal_error(
                format!("a2a task {task_id} not found"),
                None,
            ));
        }
        Prepared::Skip {
            protocol: None,
            reason,
        } => {
            return Err(McpError::invalid_params(
                format!("task {task_id}: {reason}"),
                None,
            ));
        }
        // Known pattern but nothing to validate (no transcript / no trajectory):
        // report a non-conformant verdict rather than erroring.
        Prepared::Skip {
            protocol: Some(id),
            reason,
        } => {
            return json_result(&json!({
                "task_id": params.task_id,
                "protocol": id.name(),
                "conformant": false,
                "conformance_error": reason,
                "n_events": 0,
                "events": Value::Array(Vec::new()),
            }));
        }
        Prepared::Ready(r) => r,
    };

    // MSM trend over PRIOR runs of this protocol (loaded before inserting this
    // one, so the probe is not self-matched).
    let trend = {
        let (ok, bad) = load_protocol_cohorts(pool, ready.protocol.name())
            .await
            .unwrap_or((Vec::new(), Vec::new()));
        let c = load_msm_c(pool).await.unwrap_or(DEFAULT_MSM_C);
        classify_trend(&ready.encoded, ok, bad, 3, c)
    };

    let run_trace_id = insert_run_trace(
        pool,
        task_id,
        ready.protocol.name(),
        ready.conformant,
        ready.conformance_error.as_deref(),
        &ready.trace,
        &ready.encoded,
        ready.trajectory_id,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("insert_run_trace failed: {e}"), None))?;

    json_result(&json!({
        "task_id": params.task_id,
        "protocol": ready.protocol.name(),
        "conformant": ready.conformant,
        "conformance_error": ready.conformance_error,
        "n_turns": ready.n_turns,
        "n_events": ready.trace.len(),
        "events": serde_json::to_value(&ready.trace).unwrap_or(Value::Null),
        "encoded_series": ready.encoded,
        "run_trace_id": run_trace_id,
        "trajectory_id": ready.trajectory_id,
        "msm_trend": trend.map(|(pred, sdist, fdist)| {
            json!({
                "predicted_conformant": pred,
                "mean_dist_to_conformant": sdist,
                "mean_dist_to_nonconformant": fdist,
            })
        }),
    }))
}
