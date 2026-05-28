//! The per-machine transition relation — the single pure, total chokepoint that
//! decides whether a role may take an action from a state, mirroring the
//! discipline of `src/tracker/transition.rs::check_transition` (ADR-004): one
//! authoritative function that both the (future) interpreter and the Phase-2
//! conformance observer funnel through, so they cannot diverge.

use crate::csm::machine::{LocalMachine, LocalState};
use crate::csm::role::{Action, Label};

/// What the machine knows when attempting a step. For a receive, `recv_head` is
/// the label at the head of the relevant input channel (FIFO discipline);
/// `None` enforces no FIFO constraint (synchronous/rendezvous checking).
#[derive(Debug, Clone, Copy, Default)]
pub struct StepContext<'a> {
    pub recv_head: Option<&'a Label>,
}

/// Why a step is refused. Total over the Phase-1 surface; the conformance layer
/// (Phase 2) adds network-level variants (`WrongRole`, `Deadlock`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepError {
    /// No edge from `state` performs `action`.
    NoEdge { state: LocalState },
    /// A receive whose awaited label is not the head of the input channel.
    RecvNotHead { expected: String, head: String },
}

impl StepError {
    pub fn message(&self) -> String {
        match self {
            StepError::NoEdge { state } => format!("no legal action from state {state}"),
            StepError::RecvNotHead { expected, head } => {
                format!("receive expected '{expected}' but channel head is '{head}'")
            }
        }
    }
}

/// Decide whether `machine` may perform `action` from `state`. Pure and total.
/// Returns the next state on success. For a receive, if `ctx.recv_head` is
/// supplied it must match the awaited label (FIFO).
pub fn check_step(
    machine: &LocalMachine,
    state: LocalState,
    action: &Action,
    ctx: &StepContext,
) -> Result<LocalState, StepError> {
    for e in machine.edges_from(state) {
        if &e.action == action {
            if let Action::Recv { label, .. } = action
                && let Some(head) = ctx.recv_head
                && head.name != label.name
            {
                return Err(StepError::RecvNotHead {
                    expected: label.name.clone(),
                    head: head.name.clone(),
                });
            }
            return Ok(e.to);
        }
    }
    Err(StepError::NoEdge { state })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csm::machine::compile;
    use crate::csm::mpst::local::LocalType;
    use crate::csm::role::{Label, Role};

    #[test]
    fn steps_through_a_linear_machine_to_a_terminal() {
        // !P⟨plan⟩ . ?P⟨ans⟩ . end
        let lt = LocalType::send(
            "P",
            Label::text("plan"),
            LocalType::recv("P", Label::text("ans"), LocalType::End),
        );
        let m = compile(&Role::new("O"), &lt);

        let s0 = m.initial;
        let s1 = check_step(
            &m,
            s0,
            &Action::Send {
                to: Role::new("P"),
                label: Label::text("plan"),
            },
            &StepContext::default(),
        )
        .expect("send is legal from the initial state");

        let s2 = check_step(
            &m,
            s1,
            &Action::Recv {
                from: Role::new("P"),
                label: Label::text("ans"),
            },
            &StepContext::default(),
        )
        .expect("recv is legal next");

        assert!(m.is_terminal(s2));
    }

    #[test]
    fn illegal_action_is_no_edge() {
        let lt = LocalType::send("P", Label::text("plan"), LocalType::End);
        let m = compile(&Role::new("O"), &lt);
        let err = check_step(
            &m,
            m.initial,
            &Action::Recv {
                from: Role::new("P"),
                label: Label::text("plan"),
            },
            &StepContext::default(),
        )
        .expect_err("a receive is not legal where a send is required");
        assert!(matches!(err, StepError::NoEdge { .. }));
    }

    #[test]
    fn fifo_head_mismatch_is_rejected() {
        // A branch offering two labels from O; supplying the wrong head rejects.
        let lt = LocalType::branch(
            "O",
            vec![
                crate::csm::mpst::local::lbranch(Label::text("a"), LocalType::End),
                crate::csm::mpst::local::lbranch(Label::text("b"), LocalType::End),
            ],
        );
        let m = compile(&Role::new("R"), &lt);
        let head = Label::text("b");
        // Attempt to receive 'a' while the channel head is 'b'.
        let err = check_step(
            &m,
            m.initial,
            &Action::Recv {
                from: Role::new("O"),
                label: Label::text("a"),
            },
            &StepContext {
                recv_head: Some(&head),
            },
        )
        .expect_err("FIFO head mismatch");
        assert!(matches!(err, StepError::RecvNotHead { .. }));
    }
}
