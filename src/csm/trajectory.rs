//! Phase-3 MSM bridge: encode a protocol [`Trace`] as a univariate `f64` series
//! so reified-pattern runs feed the **same** Move-Split-Merge trajectory index
//! the RLM already uses (`crate::fuzzy::trajectory_index`). "A state machine's
//! runs are trajectories" (ADR-009): conformant / non-conformant runs become
//! the success / fail cohorts that index ranks and `classify_trend` separates.
//!
//! [`Trace`]: crate::csm::conformance::Trace

use crate::csm::conformance::Event;

/// Encode one protocol event as a single `f64` ("communication signature"),
/// mirroring `rlm::encode_step`'s univariate scheme (`MsmConfig::distance` is
/// defined over `&[f64]`). It captures the message *direction* — orchestrator-
/// outbound request vs inbound response — and a stable fingerprint of the label,
/// so runs with a similar communication shape encode similarly and divergent
/// ones differ.
pub fn encode_event(ev: &Event) -> f64 {
    // "O" is the orchestration hub; outbound and inbound messages sit on
    // distinct bands so request/response asymmetry is visible to MSM.
    let dir = if ev.from.as_str() == "O" { 2.0 } else { 4.0 };
    let label = &ev.label.name;
    let fp = (label.bytes().map(|b| b as u32).sum::<u32>() % 97) as f64 * 0.02;
    dir + fp + (1.0 + label.len() as f64).ln() * 0.1
}

/// Encode a whole trace as the `f64` series stored in
/// `csm_run_traces.encoded_series` (column-compatible with
/// `agent_trajectories.encoded_series`, so the existing index consumes it).
pub fn encoded_series(trace: &[Event]) -> Vec<f64> {
    let mut v = Vec::with_capacity(trace.len());
    for ev in trace {
        v.push(encode_event(ev));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csm::conformance::TranscriptTurn;
    use crate::csm::conformance::lift_transcript;
    use crate::csm::registry::ProtocolId;

    fn turn(role: &str) -> TranscriptTurn {
        TranscriptTurn {
            round: 0,
            role: role.to_string(),
            converged: false,
        }
    }

    #[test]
    fn encoding_is_deterministic() {
        let ev = Event::new("O", "P", crate::csm::role::Label::text("plan_req"));
        assert_eq!(encode_event(&ev), encode_event(&ev));
    }

    #[test]
    fn outbound_and_inbound_sit_on_distinct_bands() {
        let out = encode_event(&Event::new("O", "P", crate::csm::role::Label::text("x")));
        let inb = encode_event(&Event::new("P", "O", crate::csm::role::Label::text("x")));
        assert!(inb > out, "inbound band must exceed outbound band");
    }

    #[test]
    fn distinct_runs_produce_distinct_series() {
        let seq = encoded_series(&lift_transcript(
            ProtocolId::Sequential,
            &[turn("Planner"), turn("Critic"), turn("Solver")],
        ));
        let dist = encoded_series(&lift_transcript(
            ProtocolId::Distillation,
            &[turn("Expert"), turn("Learner")],
        ));
        assert_ne!(seq.len(), dist.len());
        assert_eq!(seq.len(), 6); // 3 turns × (req,resp)
    }
}
