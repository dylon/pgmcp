//! Shared validate-and-prepare core for CSM run conformance.
//!
//! Reads a completed `a2a_pattern_*` run, lifts it into a protocol trace, and
//! checks conformance against the pattern's projected network — WITHOUT
//! persisting (the caller decides whether/how to insert). Extracting this from
//! the `csm_validate_run` MCP tool lets the `csm-validate` cron close the
//! learning loop without depending on an agent calling the tool. The per-pattern
//! assembly mirrors the tool exactly.

use sqlx::PgPool;
use uuid::Uuid;

use crate::csm::conformance::{Event, TranscriptTurn, check_conformance, lift_transcript};
use crate::csm::machine::Network;
use crate::csm::registry::{ProtocolId, ProtocolParams, global_of};
use crate::csm::store::{find_trajectory_for_task, read_run};
use crate::csm::trajectory::encoded_series;

/// Outcome of preparing a run for conformance validation.
pub enum Prepared {
    /// `task_id` is not present in `a2a_tasks`.
    NotFound,
    /// The task exists but cannot be validated. `protocol` is `Some` when the
    /// skill is a known pattern but the transcript / trajectory is missing, and
    /// `None` when the skill is not an `a2a_pattern_*` run at all.
    Skip {
        protocol: Option<ProtocolId>,
        reason: String,
    },
    /// A validatable run with its conformance verdict and MSM-encoded series.
    Ready(Ready),
}

/// A run ready to persist: conformance verdict + the MSM-encoded series.
pub struct Ready {
    pub protocol: ProtocolId,
    pub conformant: bool,
    pub conformance_error: Option<String>,
    pub trace: Vec<Event>,
    pub encoded: Vec<f64>,
    pub trajectory_id: Option<i64>,
    pub n_turns: usize,
}

/// Read, lift, and conformance-check a run without persisting.
pub async fn prepare_validation(pool: &PgPool, task_id: Uuid) -> Result<Prepared, String> {
    let Some((skill, turns)) = read_run(pool, task_id)
        .await
        .map_err(|e| format!("read_run failed: {e}"))?
    else {
        return Ok(Prepared::NotFound);
    };
    let skill = skill.unwrap_or_default();
    let Some(id) = ProtocolId::from_skill_id(&skill) else {
        return Ok(Prepared::Skip {
            protocol: None,
            reason: format!("skill '{skill}' is not an a2a_pattern_* run"),
        });
    };

    let n_turns = turns.len();
    let (g, trace, encoded, trajectory_id) = if id == ProtocolId::Recursive {
        let Some((tid, subcalls, rlm_series)) = find_trajectory_for_task(pool, task_id)
            .await
            .map_err(|e| format!("trajectory lookup failed: {e}"))?
        else {
            return Ok(Prepared::Skip {
                protocol: Some(id),
                reason: "no RLM trajectory recorded for this recursive task".to_string(),
            });
        };
        let depth = subcalls.max(0) as usize;
        let g = global_of(
            ProtocolId::Recursive,
            &ProtocolParams {
                rlm_depth: depth,
                ..ProtocolParams::default()
            },
        );
        // A trace of `depth` subcall turns (one per RLM sub-call).
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
            return Ok(Prepared::Skip {
                protocol: Some(id),
                reason: "no csm_transcript recorded on this task (predates transcript capture, \
                    or recording failed)"
                    .to_string(),
            });
        }
        let g = global_of(id, &ProtocolParams::default());
        let trace = lift_transcript(id, &turns);
        let encoded = encoded_series(&trace);
        (g, trace, encoded, None)
    };

    let net =
        Network::build(id.name(), &g).map_err(|e| format!("projection failed: {}", e.message()))?;
    let (conformant, conformance_error) = match check_conformance(&net, &trace) {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e.message())),
    };

    Ok(Prepared::Ready(Ready {
        protocol: id,
        conformant,
        conformance_error,
        trace,
        encoded,
        trajectory_id,
        n_turns,
    }))
}
