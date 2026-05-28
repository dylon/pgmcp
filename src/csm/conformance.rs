//! Conformance checking: replay a recorded run (a [`Trace`] of communications)
//! against a projected [`Network`] and decide whether it is a valid path through
//! the protocol. Plus [`lift_transcript`], which maps an `a2a_pattern_*` tool's
//! recorded transcript into a [`Trace`].
//!
//! Semantics are **synchronous (rendezvous)** — the faithful model of A2A's
//! blocking `tasks/send` (ADR-009): each event advances both the sender (a
//! `Send`) and the receiver (a `Recv`) through the single legality oracle
//! [`check_step`]. A run conforms iff every event is legal *and* every machine
//! finishes in a terminal state. A run that stopped mid-protocol (e.g. a
//! Deliberation that converged without delivering the protocol's final turn to
//! the Tool-Caller) is reported [`ConformanceError::Incomplete`] — a true
//! finding about where the text implementation diverges from the spec.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::csm::machine::{LocalState, Network};
use crate::csm::registry::ProtocolId;
use crate::csm::role::{Action, Label, Role};
use crate::csm::transition::{StepContext, StepError, check_step};

/// One communication event in a run: `from` sends `label` to `to`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub from: Role,
    pub to: Role,
    pub label: Label,
}

impl Event {
    pub fn new(from: impl Into<Role>, to: impl Into<Role>, label: Label) -> Self {
        Event {
            from: from.into(),
            to: to.into(),
            label,
        }
    }
}

/// A run as a sequence of communications.
pub type Trace = Vec<Event>;

/// Why a run does not conform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConformanceError {
    /// An event names a role with no machine in the network.
    UnknownRole { role: String, ord: usize },
    /// An endpoint's machine could not take the event's action.
    Step {
        ord: usize,
        role: String,
        err: StepError,
    },
    /// The run ended with a machine in a non-terminal state (a prefix, not a
    /// complete protocol path).
    Incomplete { role: String, state: LocalState },
}

impl ConformanceError {
    pub fn message(&self) -> String {
        match self {
            ConformanceError::UnknownRole { role, ord } => {
                format!("event {ord} names unknown role '{role}'")
            }
            ConformanceError::Step { ord, role, err } => {
                format!("event {ord}: role '{role}': {}", err.message())
            }
            ConformanceError::Incomplete { role, state } => {
                format!("run incomplete: role '{role}' stalled at non-terminal state {state}")
            }
        }
    }
}

/// Replay `trace` against `net`. Each event advances the sender on a `Send` and
/// the receiver on a `Recv`; the run conforms iff every step is legal and every
/// machine ends terminal.
pub fn check_conformance(net: &Network, trace: &[Event]) -> Result<(), ConformanceError> {
    let mut states: BTreeMap<Role, LocalState> = net
        .machines
        .iter()
        .map(|(r, m)| (r.clone(), m.initial))
        .collect();

    for (ord, ev) in trace.iter().enumerate() {
        // Sender performs a Send.
        let sm = net
            .machine(&ev.from)
            .ok_or_else(|| ConformanceError::UnknownRole {
                role: ev.from.to_string(),
                ord,
            })?;
        let cur = *states.get(&ev.from).expect("sender state tracked at init");
        let ns = check_step(
            sm,
            cur,
            &Action::Send {
                to: ev.to.clone(),
                label: ev.label.clone(),
            },
            &StepContext::default(),
        )
        .map_err(|err| ConformanceError::Step {
            ord,
            role: ev.from.to_string(),
            err,
        })?;
        states.insert(ev.from.clone(), ns);

        // Receiver performs the matching Recv (FIFO head = the event's label).
        let rm = net
            .machine(&ev.to)
            .ok_or_else(|| ConformanceError::UnknownRole {
                role: ev.to.to_string(),
                ord,
            })?;
        let cur_r = *states.get(&ev.to).expect("receiver state tracked at init");
        let nr = check_step(
            rm,
            cur_r,
            &Action::Recv {
                from: ev.from.clone(),
                label: ev.label.clone(),
            },
            &StepContext {
                recv_head: Some(&ev.label),
            },
        )
        .map_err(|err| ConformanceError::Step {
            ord,
            role: ev.to.to_string(),
            err,
        })?;
        states.insert(ev.to.clone(), nr);
    }

    for (role, st) in &states {
        let m = net.machine(role).expect("machine for tracked role");
        if !m.is_terminal(*st) {
            return Err(ConformanceError::Incomplete {
                role: role.to_string(),
                state: *st,
            });
        }
    }
    Ok(())
}

/// One recorded turn of an `a2a_pattern_*` run, as persisted to
/// `a2a_tasks.metadata->'csm_transcript'` by the pattern tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptTurn {
    #[serde(default)]
    pub round: u32,
    /// The acting role's display name (`"Planner"`, `"Reflector"`, `"Sub1"`, …).
    pub role: String,
    /// Deliberation only: did this Reflector turn signal convergence?
    #[serde(default)]
    pub converged: bool,
}

/// Build [`TranscriptTurn`]s from a pattern tool's in-memory transcript (entries
/// `{round, role, output}`). The `converged` flag is derived from a Reflector
/// turn whose output carries the `CONVERGED` marker (Deliberation); other
/// patterns ignore it.
pub fn transcript_to_turns(transcript: &[serde_json::Value]) -> Vec<TranscriptTurn> {
    transcript
        .iter()
        .map(|e| {
            let role = e
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let round = e.get("round").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let output = e.get("output").and_then(|v| v.as_str()).unwrap_or_default();
            let converged = role.to_lowercase().contains("reflect") && output.contains("CONVERGED");
            TranscriptTurn {
                round,
                role,
                converged,
            }
        })
        .collect()
}

/// Lift an `a2a_pattern_*` transcript into a protocol [`Trace`], synthesising the
/// orchestrator-side request/response labels each pattern's global type expects.
/// The mapping is faithful to what the tool *did* — it does not invent steps the
/// run never performed, so a divergent run yields a non-conforming trace.
pub fn lift_transcript(pattern: ProtocolId, turns: &[TranscriptTurn]) -> Trace {
    let mut tr = Trace::new();
    let o = Role::new("O");
    match pattern {
        ProtocolId::Sequential => {
            for t in turns {
                let role = t.role.to_lowercase();
                if role.contains("plan") {
                    tr.push(Event::new(o.clone(), "P", Label::text("plan_req")));
                    tr.push(Event::new("P", o.clone(), Label::text("plan")));
                } else if role.contains("crit") {
                    tr.push(Event::new(o.clone(), "C", Label::text("critique_req")));
                    tr.push(Event::new("C", o.clone(), Label::text("critique")));
                } else if role.contains("solv") {
                    tr.push(Event::new(o.clone(), "S", Label::text("solve_req")));
                    tr.push(Event::new("S", o.clone(), Label::text("solution")));
                }
            }
        }
        ProtocolId::Mixture => {
            let mut idx = 0usize;
            for t in turns {
                if t.role.to_lowercase().contains("summ") {
                    tr.push(Event::new(o.clone(), "Sum", Label::text("reduce_req")));
                    tr.push(Event::new("Sum", o.clone(), Label::text("summary")));
                } else {
                    idx += 1;
                    let sp = Role::new(format!("Sp{idx}"));
                    tr.push(Event::new(o.clone(), sp.clone(), Label::text("query")));
                    tr.push(Event::new(sp, o.clone(), Label::text("answer")));
                }
            }
        }
        ProtocolId::Distillation => {
            for t in turns {
                let role = t.role.to_lowercase();
                if role.contains("expert") {
                    tr.push(Event::new(o.clone(), "E", Label::text("query")));
                    tr.push(Event::new("E", o.clone(), Label::text("expert")));
                } else if role.contains("learn") {
                    tr.push(Event::new(o.clone(), "L", Label::text("distill_req")));
                    tr.push(Event::new("L", o.clone(), Label::text("learner")));
                }
            }
        }
        ProtocolId::Deliberation => {
            for t in turns {
                let role = t.role.to_lowercase();
                if role.contains("reflect") {
                    tr.push(Event::new(o.clone(), "R", Label::text("reflect_req")));
                    let verdict = if t.converged { "converged" } else { "continue" };
                    tr.push(Event::new("R", o.clone(), Label::text(verdict)));
                    // On convergence the protocol next has the orchestrator ask
                    // the Tool-Caller to finalise (O→T:finish.T→O:final); the
                    // text impl stops here, so the lifted trace ends and the run
                    // is reported Incomplete — the divergence the observer exists
                    // to surface.
                    if t.converged {
                        break;
                    }
                } else if role.contains("tool") {
                    tr.push(Event::new(o.clone(), "T", Label::text("act_req")));
                    tr.push(Event::new("T", o.clone(), Label::text("result")));
                }
            }
        }
        ProtocolId::Recursive => {
            let mut k = 0usize;
            for _ in turns {
                k += 1;
                let sub = Role::new(format!("Sub{k}"));
                tr.push(Event::new(o.clone(), sub.clone(), Label::text("subcall")));
                tr.push(Event::new(sub, o.clone(), Label::text("subresult")));
            }
        }
    }
    tr
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csm::machine::Network;
    use crate::csm::registry::{ProtocolId, ProtocolParams, global_of};

    fn net(id: ProtocolId, p: &ProtocolParams) -> Network {
        Network::build(id.name(), &global_of(id, p)).expect("network builds")
    }

    fn turn(role: &str) -> TranscriptTurn {
        TranscriptTurn {
            round: 0,
            role: role.to_string(),
            converged: false,
        }
    }

    #[test]
    fn sequential_real_run_conforms() {
        let p = ProtocolParams::default();
        let n = net(ProtocolId::Sequential, &p);
        let turns = [turn("Planner"), turn("Critic"), turn("Solver")];
        let trace = lift_transcript(ProtocolId::Sequential, &turns);
        check_conformance(&n, &trace)
            .unwrap_or_else(|e| panic!("sequential should conform: {}", e.message()));
    }

    #[test]
    fn distillation_real_run_conforms() {
        let p = ProtocolParams::default();
        let n = net(ProtocolId::Distillation, &p);
        let turns = [turn("Expert"), turn("Learner")];
        let trace = lift_transcript(ProtocolId::Distillation, &turns);
        check_conformance(&n, &trace).expect("distillation conforms");
    }

    #[test]
    fn mixture_real_run_conforms() {
        let p = ProtocolParams::default();
        let n = net(ProtocolId::Mixture, &p);
        let turns = [
            turn("Math"),
            turn("Code"),
            turn("Science"),
            turn("Summarizer"),
        ];
        let trace = lift_transcript(ProtocolId::Mixture, &turns);
        check_conformance(&n, &trace).expect("mixture conforms");
    }

    #[test]
    fn deliberation_converged_run_is_incomplete_missing_finalize() {
        // A Reflector-converged run stops before the protocol's O→T:finish
        // finalisation, so the Tool-Caller machine never reaches terminal.
        let p = ProtocolParams::default();
        let n = net(ProtocolId::Deliberation, &p);
        let turns = [
            turn("Reflector"),
            turn("Tool-Caller"),
            TranscriptTurn {
                round: 1,
                role: "Reflector".to_string(),
                converged: true,
            },
        ];
        let trace = lift_transcript(ProtocolId::Deliberation, &turns);
        let err = check_conformance(&n, &trace)
            .expect_err("converged text run diverges from the paper-faithful protocol");
        assert!(matches!(err, ConformanceError::Incomplete { .. }));
    }

    #[test]
    fn out_of_order_run_is_rejected() {
        // A Solver turn with no preceding Planner is not a legal protocol path.
        let p = ProtocolParams::default();
        let n = net(ProtocolId::Sequential, &p);
        let trace = lift_transcript(ProtocolId::Sequential, &[turn("Solver")]);
        let err = check_conformance(&n, &trace).expect_err("solver-first is illegal");
        assert!(matches!(
            err,
            ConformanceError::Step { .. } | ConformanceError::Incomplete { .. }
        ));
    }

    #[test]
    fn recursive_run_conforms_at_matching_depth() {
        let p = ProtocolParams {
            rlm_depth: 2,
            ..ProtocolParams::default()
        };
        let n = Network::build("recursive", &global_of(ProtocolId::Recursive, &p))
            .expect("recursive network builds");
        let turns = [turn("Sub"), turn("Sub")];
        let trace = lift_transcript(ProtocolId::Recursive, &turns);
        check_conformance(&n, &trace).expect("recursive depth-2 run conforms");
    }
}
