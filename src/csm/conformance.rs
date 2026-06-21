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

use crate::csm::machine::{EdgeKind, LocalState, Network};
use crate::csm::registry::ProtocolId;
use crate::csm::role::{Action, Label, MAX_STACK_DEPTH, Role};
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
    /// The run ended with a machine holding unreturned call frames — the pushdown
    /// stack was non-empty (an unbalanced / non-well-nested run), or a `Return`
    /// boundary was reached with no frame to pop.
    Unbalanced { role: String, depth: usize },
    /// A `Call` boundary would push past [`MAX_STACK_DEPTH`] — the run's nesting
    /// exceeds the configured bound (the decidability/termination guard).
    DepthExceeded { role: String, ord: usize },
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
            ConformanceError::Unbalanced { role, depth } => {
                format!(
                    "run unbalanced: role '{role}' ended with {depth} unreturned call frame(s) \
                     (or popped an empty stack) — the run is not well-nested"
                )
            }
            ConformanceError::DepthExceeded { role, ord } => format!(
                "event {ord}: role '{role}' would push past MAX_STACK_DEPTH={MAX_STACK_DEPTH} \
                 (call nesting exceeds the bound)"
            ),
        }
    }
}

/// A role's **pushdown configuration** during replay: its control state plus the
/// stack of pending return addresses — the call/box frames it has entered but not
/// yet returned from. For a call-free protocol the stack stays empty, so a
/// `RoleConfig` degenerates to just `state` and the pushdown replay coincides
/// exactly with the pre-pushdown finite-state replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleConfig {
    pub state: LocalState,
    pub stack: Vec<LocalState>,
}

/// Internal replay error, mapped to a [`ConformanceError`] with role/ord context.
enum ReplayErr {
    Step(StepError),
    Unbalanced,
    DepthExceeded,
}

fn map_replay_err(e: ReplayErr, role: &Role, ord: usize) -> ConformanceError {
    match e {
        ReplayErr::Step(err) => ConformanceError::Step {
            ord,
            role: role.to_string(),
            err,
        },
        ReplayErr::Unbalanced => ConformanceError::Unbalanced {
            role: role.to_string(),
            depth: 0,
        },
        ReplayErr::DepthExceeded => ConformanceError::DepthExceeded {
            role: role.to_string(),
            ord,
        },
    }
}

/// Chase a role's ε-transitions — the stack-boundary `Call` (push) and `Return`
/// (pop) edges — from `state`, mutating `stack`, until the role reaches a state
/// with no boundary edge (it is waiting for a communication, or is terminal).
/// A `Call` pushes its return address and enters the callee/box; a `Return` pops
/// the matching return address. Bounded by [`MAX_STACK_DEPTH`] (a push past the
/// bound is `DepthExceeded`; a `Return` on an empty stack is `Unbalanced`), so it
/// terminates even on a pathological no-communication recursion. This is the
/// well-nesting discipline made operational: every `Call` is matched by the
/// `Return` that pops exactly the address it pushed.
fn epsilon_close(
    m: &crate::csm::machine::LocalMachine,
    mut state: LocalState,
    stack: &mut Vec<LocalState>,
) -> Result<LocalState, ReplayErr> {
    loop {
        let mut moved = false;
        for e in m.edges_from(state) {
            match e.kind {
                EdgeKind::Call { return_state } => {
                    if stack.len() >= MAX_STACK_DEPTH {
                        return Err(ReplayErr::DepthExceeded);
                    }
                    stack.push(return_state);
                    state = e.to;
                    moved = true;
                    break;
                }
                EdgeKind::Return => {
                    let r = stack.pop().ok_or(ReplayErr::Unbalanced)?;
                    state = r;
                    moved = true;
                    break;
                }
                EdgeKind::Internal => {}
            }
        }
        if !moved {
            return Ok(state);
        }
    }
}

/// Advance one role by one communication `action`: ε-close to its waiting state,
/// take the matching `Internal` edge ([`check_step`]), then ε-close again so any
/// `Call`/`Return` boundaries immediately following are consumed and the role
/// rests at its next waiting/terminal state.
fn advance_role(
    m: &crate::csm::machine::LocalMachine,
    state: LocalState,
    stack: &mut Vec<LocalState>,
    action: &Action,
    recv_head: Option<&Label>,
) -> Result<LocalState, ReplayErr> {
    let s = epsilon_close(m, state, stack)?;
    let ns = check_step(m, s, action, &StepContext { recv_head }).map_err(ReplayErr::Step)?;
    epsilon_close(m, ns, stack)
}

/// Replay `trace` against `net`, returning each role's pushdown [`RoleConfig`]
/// (state + stack). Each event advances the sender on a `Send` and the receiver
/// on a `Recv` through the single legality oracle [`check_step`], with the
/// stack-boundary `Call`/`Return` edges taken by ε-closure on either side. This
/// is the pushdown generalization of [`replay_to_states`]: for a call-free
/// protocol every stack stays empty and the result is exactly the flat state map.
///
/// Like [`replay_to_states`], this **does not** assert termination: a mid-protocol
/// *prefix* (a paused session — possibly mid-call, with a non-empty stack) replays
/// cleanly to its configurations, which is exactly the stack-aware PAUSE/RESUME
/// recovery input ("the stack of frames IS the position").
pub fn replay_to_configs(
    net: &Network,
    trace: &[Event],
) -> Result<BTreeMap<Role, RoleConfig>, ConformanceError> {
    let mut configs: BTreeMap<Role, RoleConfig> = BTreeMap::new();
    for (r, m) in &net.machines {
        let mut stack = Vec::new();
        let state = epsilon_close(m, m.initial, &mut stack).map_err(|e| map_replay_err(e, r, 0))?;
        configs.insert(r.clone(), RoleConfig { state, stack });
    }

    for (ord, ev) in trace.iter().enumerate() {
        // Sender performs a Send.
        let sm = net
            .machine(&ev.from)
            .ok_or_else(|| ConformanceError::UnknownRole {
                role: ev.from.to_string(),
                ord,
            })?;
        {
            let cfg = configs.get_mut(&ev.from).expect("sender tracked at init");
            let action = Action::Send {
                to: ev.to.clone(),
                label: ev.label.clone(),
            };
            cfg.state = advance_role(sm, cfg.state, &mut cfg.stack, &action, None)
                .map_err(|e| map_replay_err(e, &ev.from, ord))?;
        }

        // Receiver performs the matching Recv (FIFO head = the event's label).
        let rm = net
            .machine(&ev.to)
            .ok_or_else(|| ConformanceError::UnknownRole {
                role: ev.to.to_string(),
                ord,
            })?;
        {
            let cfg = configs.get_mut(&ev.to).expect("receiver tracked at init");
            let action = Action::Recv {
                from: ev.from.clone(),
                label: ev.label.clone(),
            };
            cfg.state = advance_role(rm, cfg.state, &mut cfg.stack, &action, Some(&ev.label))
                .map_err(|e| map_replay_err(e, &ev.to, ord))?;
        }
    }

    Ok(configs)
}

/// Replay `trace` and return each role's [`LocalState`] (dropping the stack — the
/// call-free "position"). For a call-free protocol the stack is always empty, so
/// this is lossless; call-bearing PAUSE/RESUME uses [`replay_to_configs`] to keep
/// the frame stack ("the stack of frames is the position"). The returned map is
/// the resume position from which `crate::csm::driver::next_step_from` plans the
/// next step.
pub fn replay_to_states(
    net: &Network,
    trace: &[Event],
) -> Result<BTreeMap<Role, LocalState>, ConformanceError> {
    Ok(replay_to_configs(net, trace)?
        .into_iter()
        .map(|(r, c)| (r, c.state))
        .collect())
}

/// Replay `trace` against `net`. A run conforms iff every event is legal, every
/// machine ends in a terminal state, AND every machine's pushdown stack is empty
/// — i.e. the run is **well-nested** (every call returned). An unreturned call is
/// [`ConformanceError::Unbalanced`]; a mid-protocol prefix is
/// [`ConformanceError::Incomplete`].
pub fn check_conformance(net: &Network, trace: &[Event]) -> Result<(), ConformanceError> {
    let configs = replay_to_configs(net, trace)?;
    for (role, cfg) in &configs {
        if !cfg.stack.is_empty() {
            return Err(ConformanceError::Unbalanced {
                role: role.to_string(),
                depth: cfg.stack.len(),
            });
        }
        let m = net.machine(role).expect("machine for tracked role");
        if !m.is_terminal(cfg.state) {
            return Err(ConformanceError::Incomplete {
                role: role.to_string(),
                state: cfg.state,
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
        ProtocolId::RecursiveCf => {
            // The genuine pushdown RLM: a well-nested run over the self-recursive
            // `recursive_cf` protocol. `turns.len()` is the nesting depth: each
            // non-bottom level takes the `recurse` branch (and the conformance
            // engine's ε-closure PUSHES a frame on `O` and `Sub` alike), the bottom
            // level takes the `leaf` base case, then every frame POPS (an ε-closure
            // `Return`) and stitches its parent's `subresult`. The trace carries
            // ONLY the real communications — the `Call`/`Return` boundaries are
            // structural ε-moves taken by [`replay_to_configs`], so the run is
            // Dyck-balanced by construction (it leaves every stack empty iff every
            // `recurse` is matched by its `subresult` unwind).
            let sub = Role::new("Sub");
            let depth = turns.len().max(1);
            for _ in 0..depth.saturating_sub(1) {
                tr.push(Event::new(o.clone(), sub.clone(), Label::text("subcall")));
                tr.push(Event::new(o.clone(), sub.clone(), Label::text("recurse")));
            }
            tr.push(Event::new(o.clone(), sub.clone(), Label::text("subcall")));
            tr.push(Event::new(o.clone(), sub.clone(), Label::text("leaf")));
            tr.push(Event::new(sub.clone(), o.clone(), Label::text("subresult")));
            for _ in 0..depth.saturating_sub(1) {
                tr.push(Event::new(sub.clone(), o.clone(), Label::text("subresult")));
            }
        }
        ProtocolId::WorktreeNegotiation => {
            // Each turn names one typed mailbox kind exchanged between the
            // Requester (R, on the dependent) and the Editor (E, on the
            // dependency): request_worktree (R→E), then E's choice accept|decline
            // (E→R) and, on accept, moved (E→R). The mapping is 1:1 with the
            // mailbox `MessageKind`s, so a recorded coordination thread lifts
            // faithfully — a thread that skips `accept` straight to `moved`, or
            // never answers, yields a non-conforming trace the observer surfaces.
            let r = Role::new("R");
            let e = Role::new("E");
            for t in turns {
                let role = t.role.to_lowercase();
                if role.contains("request") {
                    tr.push(Event::new(
                        r.clone(),
                        e.clone(),
                        Label::text("request_worktree"),
                    ));
                } else if role.contains("accept") {
                    tr.push(Event::new(e.clone(), r.clone(), Label::text("accept")));
                } else if role.contains("moved") {
                    tr.push(Event::new(e.clone(), r.clone(), Label::text("moved")));
                } else if role.contains("decline") {
                    tr.push(Event::new(e.clone(), r.clone(), Label::text("decline")));
                }
            }
        }
        ProtocolId::TapePaging => {
            // Each turn names one paging *verb* the engine performed against the
            // working set (the turn's `role` carries the verb, mirroring how
            // WorktreeNegotiation reuses `role` for the mailbox kind). Every verb
            // is preceded by the `page_in_req . page_in_ack` handshake — one loop
            // iteration — and each looping verb is followed by its Tape→O ack:
            // get→got, put→put_ack, page_out→evicted, demote→demoted. `done` is the
            // terminal arm (handshake then the bare selection). The mapping is 1:1
            // with the engine's mechanical residency operations, so a recorded
            // paging run lifts into a conformance-checkable trace; a verb whose
            // handshake is missing (e.g. a `get` not preceded by a page-in) yields
            // a non-conforming trace the observer surfaces.
            let tape = Role::new("Tape");
            // The Tape→O acknowledgement for each looping verb (None ⇒ terminal).
            let ack_of = |verb: &str| -> Option<&'static str> {
                match verb {
                    "get" => Some("got"),
                    "put" => Some("put_ack"),
                    "page_out" => Some("evicted"),
                    "demote" => Some("demoted"),
                    _ => None,
                }
            };
            for t in turns {
                let verb = t.role.to_lowercase();
                let verb = verb.as_str();
                // Only the five protocol verbs participate; other turns are noise.
                let is_verb = matches!(verb, "get" | "put" | "page_out" | "demote" | "done");
                if !is_verb {
                    continue;
                }
                // Each iteration opens with the page-in handshake.
                tr.push(Event::new(
                    o.clone(),
                    tape.clone(),
                    Label::text("page_in_req"),
                ));
                tr.push(Event::new(
                    tape.clone(),
                    o.clone(),
                    Label::text("page_in_ack"),
                ));
                // The verb selection (O drives the choice).
                tr.push(Event::new(
                    o.clone(),
                    tape.clone(),
                    Label::text(verb.to_string()),
                ));
                // …and its acknowledgement, for the four looping arms.
                if let Some(ack) = ack_of(verb) {
                    tr.push(Event::new(tape.clone(), o.clone(), Label::text(ack)));
                } else {
                    // `done` terminates the loop — stop lifting further turns.
                    break;
                }
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

    #[test]
    fn replay_prefix_leaves_orchestrator_mid_protocol() {
        // A one-turn prefix of Sequential (the Planner round) is NOT terminal, but
        // it replays cleanly via `replay_to_states` — exactly the PAUSE input. The
        // orchestrator is left having received `plan`, awaiting the Critic round, so
        // its state is non-terminal (a `check_conformance` on the same prefix would
        // report `Incomplete`).
        let p = ProtocolParams::default();
        let n = net(ProtocolId::Sequential, &p);
        let prefix = lift_transcript(ProtocolId::Sequential, &[turn("Planner")]);
        let states = replay_to_states(&n, &prefix).expect("prefix replays cleanly");
        let o = Role::new("O");
        let m = n.machine(&o).expect("orchestrator machine");
        let st = *states.get(&o).expect("orchestrator state tracked");
        assert!(
            !m.is_terminal(st),
            "a one-round prefix must leave the orchestrator mid-protocol, was terminal at {st}"
        );
        // And the same prefix under the full terminal check is Incomplete.
        assert!(matches!(
            check_conformance(&n, &prefix),
            Err(ConformanceError::Incomplete { .. })
        ));
    }

    #[test]
    fn replay_prefix_of_every_registry_protocol_is_clean_but_non_terminal() {
        // For every choice-free registry protocol, the first orchestrator
        // request/response pair replays cleanly to a non-terminal orchestrator
        // state (the mid-protocol "position" PAUSE captures). Deliberation is
        // excluded: its first lifted turn already exercises the sender-driven
        // choice and is covered separately.
        let p = ProtocolParams::default();
        let cases = [
            (ProtocolId::Sequential, "Planner"),
            (ProtocolId::Distillation, "Expert"),
            (ProtocolId::Mixture, "Math"),
        ];
        let o = Role::new("O");
        for (id, role) in cases {
            let n = net(id, &p);
            let prefix = lift_transcript(id, &[turn(role)]);
            let states = replay_to_states(&n, &prefix)
                .unwrap_or_else(|e| panic!("{} prefix should replay: {}", id.name(), e.message()));
            let m = n.machine(&o).expect("orchestrator machine");
            let st = *states.get(&o).expect("orchestrator tracked");
            assert!(
                !m.is_terminal(st),
                "{} one-round prefix must be non-terminal for O",
                id.name()
            );
        }
    }

    #[test]
    fn replay_to_states_completes_terminal_for_a_full_run() {
        // A complete conforming run replays to all-terminal states, so the
        // terminal check `check_conformance` layers on still passes.
        let p = ProtocolParams::default();
        let n = net(ProtocolId::Sequential, &p);
        let turns = [turn("Planner"), turn("Critic"), turn("Solver")];
        let trace = lift_transcript(ProtocolId::Sequential, &turns);
        let states = replay_to_states(&n, &trace).expect("full run replays");
        for (role, st) in &states {
            let m = n.machine(role).expect("machine");
            assert!(
                m.is_terminal(*st),
                "role {role} must be terminal after a full run, was at {st}"
            );
        }
        // The terminal-asserting wrapper agrees.
        check_conformance(&n, &trace).expect("full run conforms");
    }

    #[test]
    fn replay_to_states_rejects_a_corrupt_prefix_with_step() {
        // A Solver turn with no preceding Planner is not a legal protocol path:
        // `replay_to_states` must refuse it loudly with a `Step` error (a corrupt
        // trace the resume path must NOT silently accept), not return a bogus
        // state map.
        let p = ProtocolParams::default();
        let n = net(ProtocolId::Sequential, &p);
        let corrupt = lift_transcript(ProtocolId::Sequential, &[turn("Solver")]);
        let err = replay_to_states(&n, &corrupt).expect_err("solver-first is illegal");
        assert!(
            matches!(err, ConformanceError::Step { .. }),
            "a corrupt prefix must return Step, got {err:?}"
        );
    }

    // ── TapePaging (Phase 6) ────────────────────────────────────────────────
    //
    // The paging control loop `μ loop. O→Tape:page_in_req . Tape→O:page_in_ack .
    // O→Tape{ get|put|page_out|demote : … . loop ; done : end }`. The trace is
    // built directly from `Event`s (the integration `tape_resume_lifecycle` test
    // drives the engine; here we pin the protocol's causal order).
    //
    // IMPORTANT — every loop iteration, *including the terminating one*, opens
    // with the `page_in_req . page_in_ack` handshake, because `done` is one arm of
    // the SAME sender-driven choice as the verbs. So a run that does one `get`
    // transaction and then stops must re-handshake before selecting `done`:
    // `page_in_req . page_in_ack . get . got . page_in_req . page_in_ack . done`.

    fn tp_net() -> Network {
        net(ProtocolId::TapePaging, &ProtocolParams::default())
    }

    /// One `O→Tape:page_in_req . Tape→O:page_in_ack` handshake, appended to `tr`.
    fn tp_handshake(tr: &mut Trace) {
        tr.push(Event::new("O", "Tape", Label::text("page_in_req")));
        tr.push(Event::new("Tape", "O", Label::text("page_in_ack")));
    }

    /// One verb transaction `O→Tape:verb . Tape→O:ack`, appended to `tr`.
    fn tp_verb(tr: &mut Trace, verb: &str, ack: &str) {
        tr.push(Event::new("O", "Tape", Label::text(verb)));
        tr.push(Event::new("Tape", "O", Label::text(ack)));
    }

    #[test]
    fn tape_paging_get_then_done_conforms() {
        // A full conforming run: handshake, `get`/`got`, loop, handshake, `done`.
        let n = tp_net();
        let mut trace = Trace::new();
        tp_handshake(&mut trace); // iteration 1
        tp_verb(&mut trace, "get", "got"); // … select get, loop back
        tp_handshake(&mut trace); // iteration 2
        trace.push(Event::new("O", "Tape", Label::text("done"))); // … select done → end
        check_conformance(&n, &trace)
            .unwrap_or_else(|e| panic!("get→done should conform: {}", e.message()));
    }

    #[test]
    fn tape_paging_all_verbs_then_done_conforms() {
        // Each of the four looping verbs in turn, then `done`. Exercises every
        // choice arm's loop-back plus the terminal arm.
        let n = tp_net();
        let mut trace = Trace::new();
        for (verb, ack) in [
            ("get", "got"),
            ("put", "put_ack"),
            ("page_out", "evicted"),
            ("demote", "demoted"),
        ] {
            tp_handshake(&mut trace);
            tp_verb(&mut trace, verb, ack);
        }
        tp_handshake(&mut trace);
        trace.push(Event::new("O", "Tape", Label::text("done")));
        check_conformance(&n, &trace)
            .unwrap_or_else(|e| panic!("all-verbs→done should conform: {}", e.message()));
    }

    #[test]
    fn tape_paging_get_before_page_in_is_step_rejected() {
        // Causal order: you cannot `get` a page before paging one in. A `get`
        // selection with no preceding `page_in_req . page_in_ack` handshake is not
        // a legal path — the choice is only reachable AFTER the handshake.
        let n = tp_net();
        // The very first event is the verb selection (no handshake).
        let trace = vec![Event::new("O", "Tape", Label::text("get"))];
        let err = check_conformance(&n, &trace)
            .expect_err("get-before-page_in_req is an illegal protocol path");
        assert!(
            matches!(err, ConformanceError::Step { .. }),
            "an un-paged get must be Step-rejected, got {err:?}"
        );
        // `replay_to_states` must also refuse it loudly (the resume path must not
        // silently accept a corrupt prefix).
        assert!(matches!(
            replay_to_states(&n, &trace),
            Err(ConformanceError::Step { .. })
        ));
    }

    #[test]
    fn tape_paging_page_in_req_only_prefix_replays_clean_but_non_terminal() {
        // The PAUSE input: a `page_in_req`-only prefix (the controller has asked
        // for a page-in and is awaiting the ack) replays cleanly via
        // `replay_to_states` to a NON-terminal orchestrator state, but the full
        // terminal check reports Incomplete — exactly mirroring the existing
        // prefix tests for the other protocols.
        let n = tp_net();
        let prefix = vec![Event::new("O", "Tape", Label::text("page_in_req"))];
        let states = replay_to_states(&n, &prefix).expect("page_in_req prefix replays cleanly");
        let o = Role::new("O");
        let m = n.machine(&o).expect("orchestrator machine");
        let st = *states.get(&o).expect("orchestrator state tracked");
        assert!(
            !m.is_terminal(st),
            "a page_in_req-only prefix must leave O mid-protocol, was terminal at {st}"
        );
        assert!(matches!(
            check_conformance(&n, &prefix),
            Err(ConformanceError::Incomplete { .. })
        ));
    }

    #[test]
    fn tape_paging_lift_of_get_done_turns_conforms() {
        // The lift path: a recorded paging run whose verbs are `get` then `done`
        // lifts to the same conforming trace (handshake·get·got·handshake·done).
        let n = tp_net();
        let turns = [turn("get"), turn("done")];
        let trace = lift_transcript(ProtocolId::TapePaging, &turns);
        // 4 events for the get iteration + 3 for the done iteration (handshake×2 +
        // the bare done selection).
        assert_eq!(trace.len(), 7, "get→done lifts to seven events: {trace:?}");
        check_conformance(&n, &trace)
            .unwrap_or_else(|e| panic!("lifted get→done should conform: {}", e.message()));
    }

    #[test]
    fn tape_paging_lift_ignores_non_verb_turns_and_stops_at_done() {
        // A stray non-verb turn is dropped, and turns AFTER `done` are not lifted
        // (the protocol has ended). `get`, chatter, `done`, `put` ⇒ the put is
        // never reached.
        let n = tp_net();
        let turns = [turn("get"), turn("heartbeat"), turn("done"), turn("put")];
        let trace = lift_transcript(ProtocolId::TapePaging, &turns);
        assert_eq!(
            trace.len(),
            7,
            "chatter dropped, post-done put ignored: {trace:?}"
        );
        check_conformance(&n, &trace).expect("conforms despite chatter and trailing put");
    }

    #[test]
    fn tape_paging_done_without_handshake_is_rejected() {
        // `done` is gated behind the page-in handshake (it is an arm of the choice
        // that follows `page_in_ack`). Selecting `done` as the very first event —
        // before any `page_in_req`/`page_in_ack` — is not a legal path.
        let n = tp_net();
        let trace = vec![Event::new("O", "Tape", Label::text("done"))];
        let err = check_conformance(&n, &trace)
            .expect_err("done-before-handshake diverges from the loop");
        assert!(matches!(
            err,
            ConformanceError::Step { .. } | ConformanceError::Incomplete { .. }
        ));
    }

    #[test]
    fn worktree_negotiation_accept_then_moved_conforms() {
        // R asks, E accepts, E reports moved — the happy path through the
        // accept branch (`request_worktree . accept . moved . end`).
        let p = ProtocolParams::default();
        let n = net(ProtocolId::WorktreeNegotiation, &p);
        let turns = [turn("request_worktree"), turn("accept"), turn("moved")];
        let trace = lift_transcript(ProtocolId::WorktreeNegotiation, &turns);
        check_conformance(&n, &trace)
            .unwrap_or_else(|e| panic!("accept→moved should conform: {}", e.message()));
    }

    #[test]
    fn worktree_negotiation_decline_conforms() {
        // The decline branch (`request_worktree . decline . end`) is a complete,
        // legal run: the dependent escalates or withdraws out of band.
        let p = ProtocolParams::default();
        let n = net(ProtocolId::WorktreeNegotiation, &p);
        let turns = [turn("request_worktree"), turn("decline")];
        let trace = lift_transcript(ProtocolId::WorktreeNegotiation, &turns);
        check_conformance(&n, &trace).expect("decline branch conforms");
    }

    #[test]
    fn worktree_negotiation_moved_without_accept_is_rejected() {
        // `moved` before the editor's `accept` selection is not a legal path —
        // the protocol requires the choice be announced first. The observer
        // surfaces an editor that jumps straight to "moved".
        let p = ProtocolParams::default();
        let n = net(ProtocolId::WorktreeNegotiation, &p);
        let turns = [turn("request_worktree"), turn("moved")];
        let trace = lift_transcript(ProtocolId::WorktreeNegotiation, &turns);
        let err = check_conformance(&n, &trace)
            .expect_err("moved-without-accept diverges from the protocol");
        assert!(matches!(
            err,
            ConformanceError::Step { .. } | ConformanceError::Incomplete { .. }
        ));
    }

    #[test]
    fn worktree_negotiation_request_only_is_incomplete() {
        // R asked, E never answered: the network is still awaiting E's choice, so
        // the run is Incomplete — exactly the "stalled negotiation" the observer
        // must surface (the dependent should escalate or withdraw).
        let p = ProtocolParams::default();
        let n = net(ProtocolId::WorktreeNegotiation, &p);
        let turns = [turn("request_worktree")];
        let trace = lift_transcript(ProtocolId::WorktreeNegotiation, &turns);
        let err = check_conformance(&n, &trace).expect_err("an unanswered request is incomplete");
        assert!(matches!(err, ConformanceError::Incomplete { .. }));
    }

    #[test]
    fn worktree_negotiation_accept_only_is_incomplete() {
        // E accepted but never reported `moved`: the accept branch requires a
        // trailing `moved` before terminal, so accept-without-moved is Incomplete.
        let p = ProtocolParams::default();
        let n = net(ProtocolId::WorktreeNegotiation, &p);
        let turns = [turn("request_worktree"), turn("accept")];
        let trace = lift_transcript(ProtocolId::WorktreeNegotiation, &turns);
        let err = check_conformance(&n, &trace).expect_err("accept-without-moved is incomplete");
        assert!(matches!(err, ConformanceError::Incomplete { .. }));
    }

    #[test]
    fn worktree_negotiation_lift_ignores_non_protocol_turns() {
        // A stray plain message in the thread (role outside the 4 kinds) is not
        // part of the protocol alphabet and is dropped by the lift, so the
        // request_worktree·accept·moved run still conforms despite the noise.
        let p = ProtocolParams::default();
        let n = net(ProtocolId::WorktreeNegotiation, &p);
        let turns = [
            turn("request_worktree"),
            turn("fyi-some-chatter"),
            turn("accept"),
            turn("moved"),
        ];
        let trace = lift_transcript(ProtocolId::WorktreeNegotiation, &turns);
        // Exactly three protocol events survive (the chatter is dropped).
        assert_eq!(trace.len(), 3, "non-protocol turn dropped from the lift");
        check_conformance(&n, &trace).expect("conforms despite stray chatter");
    }

    // ── Pushdown protocols: RecursiveCf (genuine recursion) + HSM box ────────
    //
    // These exercise the stack: `Call`/`Return` boundary edges are taken by the
    // ε-closure in `replay_to_configs`, and a run conforms iff it is well-nested
    // (every frame returned ⇒ every per-role stack ends empty).

    use crate::csm::examples::hsm_tool_box;
    use crate::csm::registry::protocol_env;

    fn recursive_cf_net() -> Network {
        let g = global_of(ProtocolId::RecursiveCf, &ProtocolParams::default());
        Network::build_in("recursive_cf", &g, &protocol_env()).expect("recursive_cf builds")
    }

    #[test]
    fn recursive_cf_depth_one_leaf_conforms() {
        // A single leaf level (no recursion): subcall · leaf · subresult.
        let n = recursive_cf_net();
        let trace = lift_transcript(ProtocolId::RecursiveCf, &[turn("L1")]);
        check_conformance(&n, &trace)
            .unwrap_or_else(|e| panic!("depth-1 leaf run should conform: {}", e.message()));
    }

    #[test]
    fn recursive_cf_deep_well_nested_runs_conform() {
        // Depths 2..=5 must all conform: each `recurse` pushes a frame (an ε-Call
        // on both O and Sub), the leaf bottoms out, and every frame returns — the
        // Dyck-balanced run the VPA accepts. This is the property a finite-state
        // machine cannot recognize: unbounded matched call/return nesting.
        let n = recursive_cf_net();
        for depth in 2..=5usize {
            let turns: Vec<TranscriptTurn> = (0..depth).map(|i| turn(&format!("L{i}"))).collect();
            let trace = lift_transcript(ProtocolId::RecursiveCf, &turns);
            check_conformance(&n, &trace).unwrap_or_else(|e| {
                panic!(
                    "depth-{depth} well-nested RLM run should conform: {}",
                    e.message()
                )
            });
        }
    }

    #[test]
    fn recursive_cf_missing_unwind_is_not_well_nested() {
        // A run that recurses one level, leafs, and returns ONCE but never delivers
        // the parent level's `subresult` is not a complete well-nested run: O is
        // left mid-protocol (Incomplete) — exactly the divergence the observer
        // surfaces. (Both roles still have balanced ε-stacks at the point they
        // stall, so this manifests as Incomplete, not Unbalanced.)
        let n = recursive_cf_net();
        let o = Role::new("O");
        let sub = Role::new("Sub");
        let trace = vec![
            Event::new(o.clone(), sub.clone(), Label::text("subcall")), // level 1
            Event::new(o.clone(), sub.clone(), Label::text("recurse")), // push
            Event::new(o.clone(), sub.clone(), Label::text("subcall")), // level 2
            Event::new(o.clone(), sub.clone(), Label::text("leaf")),
            Event::new(sub.clone(), o.clone(), Label::text("subresult")), // level 2 only
                                                                          // …level-1 subresult missing
        ];
        let err = check_conformance(&n, &trace)
            .expect_err("a run missing the parent-level unwind is non-conforming");
        assert!(
            matches!(
                err,
                ConformanceError::Incomplete { .. } | ConformanceError::Unbalanced { .. }
            ),
            "expected Incomplete/Unbalanced, got {err:?}"
        );
    }

    #[test]
    fn recursive_cf_subresult_before_subcall_is_step_rejected() {
        // Causal order: a `subresult` cannot precede its `subcall`. The very first
        // event being a `subresult` is an illegal protocol path.
        let n = recursive_cf_net();
        let trace = vec![Event::new("Sub", "O", Label::text("subresult"))];
        let err = check_conformance(&n, &trace).expect_err("subresult-first is illegal");
        assert!(matches!(
            err,
            ConformanceError::Step { .. } | ConformanceError::Incomplete { .. }
        ));
    }

    #[test]
    fn hsm_tool_box_run_conforms() {
        // The inline HSM box: O is a bystander to the boxed sub-region (it never
        // sees into it); W and Tool push on entry and pop on exit. A complete run
        // leaves every stack empty.
        let g = hsm_tool_box();
        let n = Network::build("hsm", &g).expect("HSM box builds with an empty env (inline box)");
        let trace = vec![
            Event::new("O", "W", Label::text("task")),
            Event::new("W", "Tool", Label::text("invoke")),
            Event::new("Tool", "W", Label::text("result")),
            Event::new("W", "O", Label::text("done")),
        ];
        check_conformance(&n, &trace)
            .unwrap_or_else(|e| panic!("HSM box run should conform: {}", e.message()));
    }

    #[test]
    fn hsm_tool_box_skipping_the_box_body_is_rejected() {
        // Jumping straight from `task` to `done` without the boxed invoke/result is
        // not a legal path: W is still inside the box (it must invoke the tool).
        let g = hsm_tool_box();
        let n = Network::build("hsm", &g).expect("builds");
        let trace = vec![
            Event::new("O", "W", Label::text("task")),
            Event::new("W", "O", Label::text("done")),
        ];
        let err = check_conformance(&n, &trace).expect_err("skipping the box body is illegal");
        assert!(matches!(
            err,
            ConformanceError::Step { .. } | ConformanceError::Incomplete { .. }
        ));
    }
}
