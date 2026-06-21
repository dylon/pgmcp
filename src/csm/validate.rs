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
use crate::csm::registry::{ProtocolId, ProtocolParams, global_of, protocol_env};
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

    // Build against the protocol environment so call-bearing protocols (RecursiveCf, and any
    // future named-GlobalCall plan) resolve their callees through the registry; call-free
    // patterns are unaffected by the populated env.
    let env = protocol_env();
    let net = Network::build_in(id.name(), &g, &env)
        .map_err(|e| format!("projection failed: {}", e.message()))?;
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

/// Conformance verdict for a `WorktreeNegotiation` coordination run (ADR-009 §4.4).
pub struct CoordinationVerdict {
    pub coordination_id: i64,
    pub status: String,
    pub n_turns: usize,
    pub conformant: bool,
    pub conformance_error: Option<String>,
    pub trace: Vec<Event>,
}

/// Lift a recorded `WorktreeNegotiation` coordination (by `coordination_requests.id`)
/// from its mailbox thread and conformance-check it against the protocol. The
/// thread is the `request_worktree` message linked on the request plus every
/// typed reply (`accept`/`decline`/`moved`) threaded under it (`reply_to`),
/// time-ordered; each typed message maps to one protocol turn. This realizes the
/// §4.4 intent — `csm_validate_run` *lifts the mailbox transcript*. No persistence:
/// a coordination is not an `a2a_tasks` run, so there is no `csm_run_traces` row.
pub async fn validate_coordination(
    pool: &PgPool,
    coordination_id: i64,
) -> Result<CoordinationVerdict, String> {
    let row: Option<(Option<i64>, String)> =
        sqlx::query_as("SELECT message_id, status FROM coordination_requests WHERE id = $1")
            .bind(coordination_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("read coordination failed: {e}"))?;
    let Some((message_id, status)) = row else {
        return Err(format!("coordination #{coordination_id} not found"));
    };

    // Gather the thread: the request message + its typed replies, time-ordered.
    let kinds: Vec<String> = match message_id {
        Some(mid) => sqlx::query_scalar(
            "SELECT kind FROM agent_messages
              WHERE id = $1 OR reply_to = $1
              ORDER BY created_at, id",
        )
        .bind(mid)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("read thread failed: {e}"))?,
        None => Vec::new(),
    };

    // Only the four typed protocol kinds become turns; any plain message/fyi the
    // agents exchanged in the same thread is not part of the protocol alphabet.
    let turns: Vec<TranscriptTurn> = kinds
        .iter()
        .filter(|k| {
            matches!(
                k.as_str(),
                "request_worktree" | "accept" | "decline" | "moved"
            )
        })
        .map(|k| TranscriptTurn {
            round: 0,
            role: k.clone(),
            converged: false,
        })
        .collect();

    let g = global_of(ProtocolId::WorktreeNegotiation, &ProtocolParams::default());
    let env = protocol_env();
    let net = Network::build_in(ProtocolId::WorktreeNegotiation.name(), &g, &env)
        .map_err(|e| format!("projection failed: {}", e.message()))?;
    let trace = lift_transcript(ProtocolId::WorktreeNegotiation, &turns);
    let (conformant, conformance_error) = match check_conformance(&net, &trace) {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e.message())),
    };
    Ok(CoordinationVerdict {
        coordination_id,
        status,
        n_turns: turns.len(),
        conformant,
        conformance_error,
        trace,
    })
}
