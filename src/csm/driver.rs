//! Protocol interpreter (ADR-009 Phase 6). The `PatternDriver` seam selects how
//! a coordination pattern is orchestrated:
//!
//! - [`HardcodedDriver`] — the existing per-tool async order (the default).
//! - [`ProtocolDriver`] — derive the order from the CFSM/MPST protocol itself.
//!   `plan` walks the orchestrator role's compiled machine (the edges built by
//!   `csm::machine::compile`, i.e. the transitions `csm::transition::check_step`
//!   enforces) and returns the prescribed `(peer, request, response)` sequence.
//!   A run executed in this order is conformant by construction.
//!
//! It is a trait object, never a cfg (the project has no `[features]`), gated at
//! runtime by `[a2a] protocol_interpreter`. `plan` applies to the choice-free
//! (linear) patterns — Sequential, Mixture, Distillation, Recursive — where the
//! orchestrator drives a deterministic request/response chain. Deliberation's
//! sender-driven choice is resolved at runtime, not statically, so `plan`
//! returns `None` for it (it keeps the hardcoded path); that case is covered by
//! TLC + the conformance observer.

use crate::csm::conformance::{Event, check_conformance, lift_transcript, transcript_to_turns};
use crate::csm::machine::{LocalState, Network};
use crate::csm::registry::{ProtocolId, ProtocolParams, global_of};
use crate::csm::role::{Action, Label, Role};

/// The orchestration seam.
pub trait PatternDriver {
    fn name(&self) -> &'static str;
}

/// The default: each pattern tool's own hardcoded async order.
pub struct HardcodedDriver;

impl PatternDriver for HardcodedDriver {
    fn name(&self) -> &'static str {
        "hardcoded"
    }
}

/// Drive the order from the protocol.
pub struct ProtocolDriver;

impl PatternDriver for ProtocolDriver {
    fn name(&self) -> &'static str {
        "protocol"
    }
}

/// One orchestrator-prescribed step: send `request` to `peer`, then receive
/// `response` from `peer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedStep {
    pub peer: Role,
    pub request: Label,
    pub response: Label,
}

impl ProtocolDriver {
    /// Extract the orchestrator's prescribed `(peer, request, response)` sequence
    /// from a protocol by walking `orchestrator`'s compiled machine. Returns
    /// `None` if the machine is not a clean linear request/response chain (e.g.
    /// it branches on a choice the orchestrator resolves at runtime).
    pub fn plan(net: &Network, orchestrator: &Role) -> Option<Vec<PlannedStep>> {
        let m = net.machine(orchestrator)?;
        let mut steps = Vec::new();
        let mut state = m.initial;
        // Bound the walk by the edge count (a linear chain visits each edge once);
        // guards against an unexpected cycle.
        let max = m.edges.len() + 1;
        for _ in 0..=max {
            if m.is_terminal(state) {
                return Some(steps);
            }
            let outs: Vec<_> = m.edges_from(state).collect();
            if outs.len() != 1 {
                return None; // a choice/branch — not a static linear chain
            }
            let send = outs[0];
            let (peer, request) = match &send.action {
                Action::Send { to, label } => (to.clone(), label.clone()),
                Action::Recv { .. } => return None, // orchestrator must lead with a send
            };
            let mids: Vec<_> = m.edges_from(send.to).collect();
            if mids.len() != 1 {
                return None;
            }
            let recv = mids[0];
            let response = match &recv.action {
                Action::Recv { from, label } if *from == peer => label.clone(),
                _ => return None,
            };
            steps.push(PlannedStep {
                peer,
                request,
                response,
            });
            state = recv.to;
        }
        None // exceeded the bound — not a finite linear chain
    }

    /// The single orchestrator-prescribed step *from an arbitrary state* of the
    /// orchestrator's machine — the resume primitive. Generalises [`plan`]'s
    /// edge-walk (which always starts at `m.initial`) to begin at `state`, the
    /// position recovered by `crate::csm::conformance::replay_to_states` for a
    /// paused session.
    ///
    /// Returns the next `(peer, request, response)` [`PlannedStep`] the
    /// orchestrator must drive, or `None` when:
    /// - `state` is terminal (nothing left to do), or
    /// - the orchestrator faces a `Choice`/`Branch` at `state` — i.e. it has more
    ///   than one outgoing edge, the **Critic loop** (`verify_req` → `pass`/`revise`)
    ///   whose branch is resolved at runtime, not statically. The caller (the
    ///   resume tool) renders that as a `critic_verdict` await instead of a step.
    ///
    /// `None` is deliberately *coarse* (terminal vs. choice vs. malformed all
    /// collapse to "no single next step"); the caller disambiguates terminal from
    /// choice by re-checking `m.is_terminal(state)` / the edge count, which it
    /// already holds the network for.
    pub fn next_step_from(
        net: &Network,
        orchestrator: &Role,
        state: LocalState,
    ) -> Option<PlannedStep> {
        let m = net.machine(orchestrator)?;
        if m.is_terminal(state) {
            return None;
        }
        let outs: Vec<_> = m.edges_from(state).collect();
        if outs.len() != 1 {
            return None; // a choice/branch (the Critic loop) — resolved at runtime
        }
        let send = outs[0];
        let (peer, request) = match &send.action {
            Action::Send { to, label } => (to.clone(), label.clone()),
            Action::Recv { .. } => return None, // orchestrator must lead with a send
        };
        let mids: Vec<_> = m.edges_from(send.to).collect();
        if mids.len() != 1 {
            return None;
        }
        let recv = mids[0];
        let response = match &recv.action {
            Action::Recv { from, label } if *from == peer => label.clone(),
            _ => return None,
        };
        Some(PlannedStep {
            peer,
            request,
            response,
        })
    }

    /// The orchestrator-side trace a plan induces (for conformance confirmation):
    /// each step is `O→peer:request` then `peer→O:response`.
    pub fn plan_trace(orchestrator: &Role, plan: &[PlannedStep]) -> Vec<Event> {
        let mut tr = Vec::with_capacity(plan.len() * 2);
        for s in plan {
            tr.push(Event::new(
                orchestrator.clone(),
                s.peer.clone(),
                s.request.clone(),
            ));
            tr.push(Event::new(
                s.peer.clone(),
                orchestrator.clone(),
                s.response.clone(),
            ));
        }
        tr
    }
}

/// The `protocol` block a collaboration-pattern tool adds to its result.
/// Execution stays hardcoded; when `[a2a] protocol_interpreter` is on, the
/// interpreter additionally lifts the recorded run and checks it against the
/// protocol, reporting conformance (ADR-009 Phase 6). For Deliberation a
/// *converged* run is reported non-conformant — the text impl skips the
/// protocol's final Tool-Caller turn, the spec/impl divergence the observer
/// exists to surface.
pub fn driver_report(
    pattern: ProtocolId,
    transcript: &[serde_json::Value],
    interpreter_on: bool,
) -> serde_json::Value {
    if !interpreter_on {
        return serde_json::json!({ "mode": "hardcoded", "protocol_interpreter": false });
    }
    let turns = transcript_to_turns(transcript);
    let g = global_of(pattern, &ProtocolParams::default());
    let (conformant, err): (bool, Option<String>) = match Network::build(pattern.name(), &g) {
        Ok(net) => match check_conformance(&net, &lift_transcript(pattern, &turns)) {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e.message())),
        },
        Err(e) => (false, Some(e.message())),
    };
    serde_json::json!({
        "mode": "hardcoded",
        "protocol_interpreter": true,
        "protocol": pattern.name(),
        "conformant": conformant,
        "conformance_error": err,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csm::conformance::check_conformance;
    use crate::csm::registry::{ProtocolId, ProtocolParams, global_of};

    fn net(id: ProtocolId, p: &ProtocolParams) -> Network {
        Network::build(id.name(), &global_of(id, p)).expect("network builds")
    }

    #[test]
    fn sequential_plan_is_planner_critic_solver() {
        let p = ProtocolParams::default();
        let plan = ProtocolDriver::plan(&net(ProtocolId::Sequential, &p), &Role::new("O"))
            .expect("sequential is linearly drivable");
        let peers: Vec<String> = plan.iter().map(|s| s.peer.to_string()).collect();
        assert_eq!(peers, vec!["P", "C", "S"]);
        assert_eq!(plan[0].request.name, "plan_req");
        assert_eq!(plan[0].response.name, "plan");
    }

    #[test]
    fn next_step_from_initial_reproduces_plan_first_step() {
        // Seeding `next_step_from` at the orchestrator's initial state must
        // reproduce `plan()[0]` — the resume primitive is consistent with the
        // from-scratch planner.
        let p = ProtocolParams::default();
        let n = net(ProtocolId::Sequential, &p);
        let o = Role::new("O");
        let m = n.machine(&o).expect("orchestrator machine");
        let plan = ProtocolDriver::plan(&n, &o).expect("sequential is drivable");
        let from_initial =
            ProtocolDriver::next_step_from(&n, &o, m.initial).expect("a first step exists");
        assert_eq!(
            from_initial, plan[0],
            "seeding at m.initial must reproduce plan()[0]"
        );
    }

    #[test]
    fn next_step_from_mid_chain_yields_the_subsequent_step() {
        // Walk the orchestrator's machine one full request/response pair forward
        // (Planner round), then `next_step_from` at that mid-state must yield the
        // SECOND plan step (the Critic round) — generalising the edge-walk to an
        // arbitrary seed.
        let p = ProtocolParams::default();
        let n = net(ProtocolId::Sequential, &p);
        let o = Role::new("O");
        let m = n.machine(&o).expect("orchestrator machine");
        let plan = ProtocolDriver::plan(&n, &o).expect("sequential is drivable");
        assert!(
            plan.len() >= 2,
            "sequential has at least planner+critic steps"
        );

        // Advance: O sends plan_req (state→send.to), then receives plan
        // (send.to→recv.to). That recv.to is the mid-chain state.
        let send = m.edges_from(m.initial).next().expect("a send edge");
        let recv = m.edges_from(send.to).next().expect("a recv edge");
        let mid = recv.to;

        let next = ProtocolDriver::next_step_from(&n, &o, mid).expect("a subsequent step exists");
        assert_eq!(
            next, plan[1],
            "seeding mid-chain must yield the correct subsequent step (plan[1])"
        );
    }

    #[test]
    fn next_step_from_terminal_is_none() {
        // At a terminal state there is no next step.
        let p = ProtocolParams::default();
        let n = net(ProtocolId::Distillation, &p);
        let o = Role::new("O");
        let m = n.machine(&o).expect("orchestrator machine");
        let terminal = *m.terminals.iter().next().expect("a terminal state exists");
        assert!(ProtocolDriver::next_step_from(&n, &o, terminal).is_none());
    }

    #[test]
    fn next_step_from_returns_none_at_a_choice() {
        // The Critic-gated synthesized loop puts the orchestrator at a Choice
        // (verify_req → pass/revise). `next_step_from` returns None there (the
        // caller renders a critic_verdict await). Built via the same fold the
        // synthesize tool uses: a single worker + a critic.
        use crate::csm::mpst::global::{self, GlobalType};
        use crate::csm::role::Label;

        fn worker_chain(o: &Role, w: &Role, tail: GlobalType) -> GlobalType {
            global::interaction(
                o.clone(),
                w.clone(),
                Label::text("t0_req"),
                global::interaction(w.clone(), o.clone(), Label::text("t0_done"), tail),
            )
        }
        let o = Role::new("O");
        let w = Role::new("W0");
        let c = Role::new("C");
        let loop_node = global::rec(
            "loop",
            global::interaction(
                o.clone(),
                c.clone(),
                Label::text("verify_req"),
                global::choice(
                    c.clone(),
                    o.clone(),
                    vec![
                        global::gbranch(
                            Label::text("pass"),
                            global::interaction(
                                o.clone(),
                                w.clone(),
                                Label::text("t0_release"),
                                global::end(),
                            ),
                        ),
                        global::gbranch(
                            Label::text("revise"),
                            worker_chain(&o, &w, global::var("loop")),
                        ),
                    ],
                ),
            ),
        );
        let g = worker_chain(&o, &w, loop_node);
        let net = Network::build("test_critic", &g).expect("network builds");
        let m = net.machine(&o).expect("orchestrator machine");

        // Walk the initial worker round (O→W:t0_req, W→O:t0_done) to reach the
        // verify_req send; one more send+recv pair reaches the Choice state.
        let s1 = m.edges_from(m.initial).next().expect("t0_req send");
        let r1 = m.edges_from(s1.to).next().expect("t0_done recv");
        // r1.to is the verify_req send state; next_step_from here is still a single
        // send (verify_req), so advance once more to land on the Choice.
        let verify_send = m.edges_from(r1.to).next().expect("verify_req send");
        let choice_state = verify_send.to;
        // At the choice state the orchestrator faces pass/revise (>1 outgoing edge).
        assert!(
            m.edges_from(choice_state).count() > 1,
            "the choice state must branch"
        );
        assert!(
            ProtocolDriver::next_step_from(&net, &o, choice_state).is_none(),
            "next_step_from must be None at the Critic choice"
        );
    }

    #[test]
    fn linear_patterns_are_drivable_and_their_plan_conforms() {
        let p = ProtocolParams::default();
        for id in [
            ProtocolId::Sequential,
            ProtocolId::Mixture,
            ProtocolId::Distillation,
            ProtocolId::Recursive,
        ] {
            let n = net(id, &p);
            let plan = ProtocolDriver::plan(&n, &Role::new("O"))
                .unwrap_or_else(|| panic!("{} should be linearly drivable", id.name()));
            // Executing the plan in order yields a conforming run by construction.
            let trace = ProtocolDriver::plan_trace(&Role::new("O"), &plan);
            check_conformance(&n, &trace)
                .unwrap_or_else(|e| panic!("{} plan must conform: {}", id.name(), e.message()));
        }
    }

    #[test]
    fn deliberation_is_not_statically_drivable() {
        // The sender-driven choice (R decides converge/continue) cannot be a
        // static linear plan — it keeps the hardcoded path.
        let p = ProtocolParams::default();
        assert!(
            ProtocolDriver::plan(&net(ProtocolId::Deliberation, &p), &Role::new("O")).is_none()
        );
    }

    #[test]
    fn driver_report_off_is_inert_on_validates() {
        use serde_json::json;
        let transcript = vec![
            json!({"round":0, "role":"Planner", "output":"p"}),
            json!({"round":0, "role":"Critic", "output":"c"}),
            json!({"round":0, "role":"Solver", "output":"s"}),
        ];
        let off = super::driver_report(ProtocolId::Sequential, &transcript, false);
        assert_eq!(off["protocol_interpreter"], json!(false));
        let on = super::driver_report(ProtocolId::Sequential, &transcript, true);
        assert_eq!(on["protocol_interpreter"], json!(true));
        assert_eq!(on["conformant"], json!(true));
    }
}
