//! Communicating State Machines (CSM): the explicit transition-system model of
//! A2A multi-agent coordination, specified via Multiparty Session Types.
//!
//! See ADR-009 (`docs/decisions/009-a2a-coordination-state-machines.md`) and the
//! design plan `~/.claude/plans/we-have-modeled-the-compiled-ritchie.md`.
//!
//! Layers (Phase 1 — model skeleton, wired but with no runtime behaviour change):
//! - [`role`] — the CFSM vocabulary (roles, labels, channels, actions, media).
//! - [`mpst`] — global/local types, well-formedness, projection (`G ↾ r`).
//! - [`machine`] — compiling a local type to a [`machine::LocalMachine`] and
//!   assembling a [`machine::Network`] (one machine per role + channel topology).
//! - [`transition`] — the pure-total per-machine [`transition::check_step`]
//!   (the single legality oracle, à la `tracker::transition::check_transition`).
//! - [`examples`] — worked-example protocols (Phase 1: Deliberation).

// Phase-1 skeleton: the module is wired into the crate but nothing *calls* it
// yet — the Phase-2 conformance observer (`csm_validate_run`) is its first
// consumer. Until then the public surface is exercised only by `#[cfg(test)]`,
// which does not count for dead-code analysis. Mirrors the `#[allow(dead_code)]`
// treatment of other staged modules (`fuzzy`, `neural`, `wfst`) in `lib.rs`.
// Remove this allow in Phase 2 once the observer references the API.
#![allow(dead_code)]

pub mod conformance;
pub mod driver;
pub mod examples;
pub mod inference;
pub mod machine;
pub mod media;
pub mod mpst;
pub mod registry;
pub mod role;
pub mod session_store;
pub mod store;
pub mod string_diagram;
pub mod tla_export;
pub mod trajectory;
pub mod transition;
pub mod validate;

#[cfg(test)]
mod golden_tests {
    //! Pin each protocol's projected-network topology so the hand-aligned TLA⁺
    //! specs (`docs/formal/tla/`) cannot silently drift from the Rust model. If
    //! a `(roles, channels)` count changes here, the corresponding `.tla` spec
    //! must be re-reviewed (and re-TLC'd).

    use super::machine::Network;
    use super::registry::{ProtocolId, ProtocolParams, global_of};

    fn topology(id: ProtocolId, p: &ProtocolParams) -> (usize, usize) {
        let net = Network::build(id.name(), &global_of(id, p)).expect("network builds");
        (net.machines.len(), net.channels.len())
    }

    #[test]
    fn protocol_topologies_match_tla_specs() {
        // Default params: n_specialists = 3, recursion_rounds = 1, rlm_depth = 2.
        let p = ProtocolParams::default();
        // (roles, directed channels) — mirrored by docs/formal/tla/.
        assert_eq!(topology(ProtocolId::Sequential, &p), (4, 6)); // O,P,C,S
        assert_eq!(topology(ProtocolId::Mixture, &p), (5, 8)); // O,Sp1..3,Sum
        assert_eq!(topology(ProtocolId::Distillation, &p), (3, 4)); // O,E,L
        assert_eq!(topology(ProtocolId::Deliberation, &p), (3, 4)); // O,R,T
        assert_eq!(topology(ProtocolId::Recursive, &p), (3, 4)); // O,Sub1,Sub2
    }
}

#[cfg(test)]
mod worked_example_tests {
    //! Phase-1 acceptance: the Deliberation protocol is well-formed, projects
    //! onto every role (the Tool-Caller bystander via the external-choice
    //! merge), builds a network, and a hand-constructed conforming run replays
    //! through `check_step` to a terminal state.

    use super::examples::deliberation;
    use super::machine::Network;
    use super::mpst::local::LocalType;
    use super::mpst::project::project;
    use super::mpst::wellformed::well_formed;
    use super::role::{Action, Channel, Label, Role};
    use super::transition::{StepContext, check_step};

    fn r(name: &str) -> Role {
        Role::new(name)
    }

    #[test]
    fn deliberation_is_well_formed() {
        well_formed(&deliberation()).expect("deliberation protocol is well-formed");
    }

    #[test]
    fn deliberation_projects_onto_every_role() {
        let g = deliberation();
        for role in ["O", "R", "T"] {
            project(&g, &r(role))
                .unwrap_or_else(|e| panic!("projection onto {role} failed: {}", e.message()));
        }
    }

    #[test]
    fn tool_caller_bystander_projects_via_external_choice_merge() {
        // T is a bystander to the R → O choice; its projection only exists
        // because the two branches' receives-from-O merge into one Branch.
        let lt = project(&deliberation(), &r("T")).expect("T projects");
        match lt {
            // μ t. &O{ finish: …; act_req: … }
            LocalType::Rec { body, .. } => match *body {
                LocalType::Branch { from, branches } => {
                    assert_eq!(from, r("O"));
                    assert_eq!(branches.len(), 2, "merge must union both branch labels");
                    let names: Vec<_> = branches.iter().map(|b| b.label.name.clone()).collect();
                    assert!(names.contains(&"finish".to_string()));
                    assert!(names.contains(&"act_req".to_string()));
                }
                other => panic!("expected a Branch under the Rec, got {other:?}"),
            },
            other => panic!("expected T's projection to be a Rec, got {other:?}"),
        }
    }

    #[test]
    fn deliberation_network_has_three_machines_and_four_channels() {
        let net = Network::build("deliberation", &deliberation()).expect("network builds");
        assert_eq!(net.machines.len(), 3);
        for role in ["O", "R", "T"] {
            assert!(
                net.machine(&r(role)).is_some(),
                "missing machine for {role}"
            );
        }
        // O→R, R→O, O→T, T→O.
        assert_eq!(net.channels.len(), 4);
        for (from, to) in [("O", "R"), ("R", "O"), ("O", "T"), ("T", "O")] {
            assert!(
                net.channels.contains(&Channel::new(r(from), r(to))),
                "missing channel {from}→{to}"
            );
        }
    }

    #[test]
    fn orchestrator_replays_a_continue_then_converge_run_to_a_terminal() {
        // compile ∘ project replays a real run with no StepError — the Phase-1
        // fidelity criterion. The Orchestrator does one `continue` round (drive
        // the Tool-Caller, loop) then a `converged` round (collect final, end).
        let net = Network::build("deliberation", &deliberation()).expect("network");
        let m = net.machine(&r("O")).expect("orchestrator machine");

        let run = [
            Action::Send {
                to: r("R"),
                label: Label::text("reflect_req"),
            },
            Action::Recv {
                from: r("R"),
                label: Label::text("continue"),
            },
            Action::Send {
                to: r("T"),
                label: Label::text("act_req"),
            },
            Action::Recv {
                from: r("T"),
                label: Label::text("result"),
            },
            Action::Send {
                to: r("R"),
                label: Label::text("reflect_req"),
            },
            Action::Recv {
                from: r("R"),
                label: Label::text("converged"),
            },
            Action::Send {
                to: r("T"),
                label: Label::text("finish"),
            },
            Action::Recv {
                from: r("T"),
                label: Label::text("final"),
            },
        ];

        let mut state = m.initial;
        for (i, action) in run.iter().enumerate() {
            state = check_step(m, state, action, &StepContext::default())
                .unwrap_or_else(|e| panic!("step {i} ({action:?}) refused: {}", e.message()));
        }
        assert!(
            m.is_terminal(state),
            "the run must end in a terminal state, ended at {state}"
        );
    }
}
