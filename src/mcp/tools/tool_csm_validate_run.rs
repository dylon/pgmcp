//! `csm_validate_run` — the keystone observer. Lift a completed `a2a_pattern_*`
//! run (by `a2a_tasks` id) into a protocol trace, check it against the pattern's
//! projected network, persist the verdict + MSM-encoded series to
//! `csm_run_traces`, and report where the run sits relative to prior conformant
//! / non-conformant runs (the Phase-3 MSM bridge).
//!
//! Recursive runs are captured by the RLM as `agent_trajectories`; this tool
//! reuses that trajectory (its subcall count fixes the protocol depth, its
//! `encoded_series` is the run's series) and links `trajectory_id`.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::context::SystemContext;
use crate::csm::conformance::{TranscriptTurn, check_conformance, lift_transcript};
use crate::csm::machine::Network;
use crate::csm::registry::{ProtocolId, ProtocolParams, global_of};
use crate::csm::store::{
    find_trajectory_for_task, insert_run_trace, load_protocol_cohorts, read_run,
};
use crate::csm::trajectory::encoded_series;
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

    let (skill, turns) = read_run(pool, task_id)
        .await
        .map_err(|e| McpError::internal_error(format!("read_run failed: {e}"), None))?
        .ok_or_else(|| McpError::internal_error(format!("a2a task {task_id} not found"), None))?;

    let skill = skill.unwrap_or_default();
    let id = ProtocolId::from_skill_id(&skill).ok_or_else(|| {
        McpError::invalid_params(
            format!("task {task_id} skill '{skill}' is not an a2a_pattern_* run"),
            None,
        )
    })?;

    // Assemble (global type, trace, encoded series, trajectory link) per pattern.
    let (g, trace, encoded, trajectory_id) = if id == ProtocolId::Recursive {
        let traj = find_trajectory_for_task(pool, task_id).await.map_err(|e| {
            McpError::internal_error(format!("trajectory lookup failed: {e}"), None)
        })?;
        let Some((tid, subcalls, rlm_series)) = traj else {
            return json_result(&json!({
                "task_id": params.task_id,
                "protocol": id.name(),
                "conformant": false,
                "conformance_error": "no RLM trajectory recorded for this recursive task",
                "n_events": 0,
                "events": Value::Array(Vec::new()),
            }));
        };
        let depth = subcalls.max(0) as usize;
        let g = global_of(
            ProtocolId::Recursive,
            &ProtocolParams {
                rlm_depth: depth,
                ..ProtocolParams::default()
            },
        );
        // A trace of `depth` subcall pairs (one per RLM sub-call).
        let synth: Vec<TranscriptTurn> = (0..depth)
            .map(|_| TranscriptTurn {
                round: 0,
                role: "Sub".to_string(),
                converged: false,
            })
            .collect();
        let trace = lift_transcript(ProtocolId::Recursive, &synth);
        // Prefer the RLM's own encoded series (column-compatible); fall back to
        // encoding the lifted trace if the trajectory had none.
        let encoded = if rlm_series.is_empty() {
            encoded_series(&trace)
        } else {
            rlm_series
        };
        (g, trace, encoded, Some(tid))
    } else {
        if turns.is_empty() {
            return json_result(&json!({
                "task_id": params.task_id,
                "protocol": id.name(),
                "conformant": false,
                "conformance_error": "no csm_transcript recorded on this task (the pattern run \
                    predates transcript capture, or recording failed)",
                "n_turns": 0,
                "events": Value::Array(Vec::new()),
            }));
        }
        let g = global_of(id, &ProtocolParams::default());
        let trace = lift_transcript(id, &turns);
        let encoded = encoded_series(&trace);
        (g, trace, encoded, None)
    };

    let net = Network::build(id.name(), &g).map_err(|e| {
        McpError::internal_error(format!("projection failed: {}", e.message()), None)
    })?;
    let (conformant, err_msg) = match check_conformance(&net, &trace) {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e.message())),
    };

    // MSM trend over PRIOR runs of this protocol (loaded before inserting this
    // one, so the probe is not self-matched).
    let trend = {
        let (ok, bad) = load_protocol_cohorts(pool, id.name())
            .await
            .unwrap_or((Vec::new(), Vec::new()));
        let c = load_msm_c(pool).await.unwrap_or(DEFAULT_MSM_C);
        classify_trend(&encoded, ok, bad, 3, c)
    };

    let run_trace_id = insert_run_trace(
        pool,
        task_id,
        id.name(),
        conformant,
        err_msg.as_deref(),
        &trace,
        &encoded,
        trajectory_id,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("insert_run_trace failed: {e}"), None))?;

    json_result(&json!({
        "task_id": params.task_id,
        "protocol": id.name(),
        "conformant": conformant,
        "conformance_error": err_msg,
        "n_turns": turns.len(),
        "n_events": trace.len(),
        "events": serde_json::to_value(&trace).unwrap_or(Value::Null),
        "encoded_series": encoded,
        "run_trace_id": run_trace_id,
        "trajectory_id": trajectory_id,
        "msm_trend": trend.map(|(pred, sdist, fdist)| {
            json!({
                "predicted_conformant": pred,
                "mean_dist_to_conformant": sdist,
                "mean_dist_to_nonconformant": fdist,
            })
        }),
    }))
}
