//! Worked-example protocols. Phase 1 ships **Deliberation** — the only one of
//! RecursiveMAS's five patterns with a genuine sender-driven choice (and hence
//! a bystander that exercises the external-choice merge) — as a `GlobalType`
//! literal, to validate the whole CSM/MPST pipeline end-to-end. Phase 2's
//! `registry.rs` supersedes this with builders for all five patterns.

use crate::csm::mpst::global::{GlobalType, choice, end, gbranch, interaction, rec, var};
use crate::csm::role::Label;

/// The Deliberation pattern (`tool_a2a_pattern_deliberation.rs`), with roles
/// `O` = Orchestrator, `R` = Reflector, `T` = Tool-Caller:
///
/// ```text
/// μ t.
///   O → R : reflect_req .
///   R → O { converged: O → T : finish  . T → O : final  . end
///         ; continue : O → T : act_req . T → O : result . t   }
/// ```
///
/// The Reflector decides convergence — the sender-driven choice `R → O`. On
/// `continue` the Orchestrator drives the Tool-Caller and loops; on `converged`
/// it collects the final answer and ends. The Tool-Caller is a **bystander** to
/// the choice (it is neither `R` nor `O`), so its projection only exists thanks
/// to the external-choice merge — making this the right hardest-first Phase-1
/// validation of the projector.
pub fn deliberation() -> GlobalType {
    rec(
        "t",
        interaction(
            "O",
            "R",
            Label::text("reflect_req"),
            choice(
                "R",
                "O",
                vec![
                    gbranch(
                        Label::text("converged"),
                        interaction(
                            "O",
                            "T",
                            Label::text("finish"),
                            interaction("T", "O", Label::text("final"), end()),
                        ),
                    ),
                    gbranch(
                        Label::text("continue"),
                        interaction(
                            "O",
                            "T",
                            Label::text("act_req"),
                            interaction("T", "O", Label::text("result"), var("t")),
                        ),
                    ),
                ],
            ),
        ),
    )
}

/// The **WorktreeNegotiation** coordination protocol (ADR-009 Phase 4), with
/// roles `R` = Requester (the agent on the blocked dependent project `D`) and
/// `E` = Editor (the agent on the dependency project `U`):
///
/// ```text
/// R → E : request_worktree .
/// E → R { accept  : E → R : moved . end
///       ; decline : end                  }
/// ```
///
/// The Editor decides — the sender-driven choice `E → R`. On `accept` it later
/// reports `moved` (it ran `git worktree add` and restored `U`'s stable branch);
/// on `decline` the Requester escalates or withdraws. pgmcp does **not** appear
/// as a protocol role: the actual unblock of `D` is not a message between `R`
/// and `E` but a separate, System-only action gated on pgmcp's git scanner
/// observing `U` stable — the trust boundary proven in
/// `docs/formal/WorktreeNegotiation.{tla,v}`. The protocol between the two
/// agents therefore ends at `moved`; the gatekeeper closes the loop out of band.
///
/// The four labels are exactly the typed A2A mailbox kinds
/// (`request_worktree`/`accept`/`moved`/`decline`), so a recorded coordination
/// thread lifts into a conformance-checkable trace
/// (`super::conformance::lift_transcript`).
pub fn worktree_negotiation() -> GlobalType {
    interaction(
        "R",
        "E",
        Label::text("request_worktree"),
        choice(
            "E",
            "R",
            vec![
                gbranch(
                    Label::text("accept"),
                    interaction("E", "R", Label::text("moved"), end()),
                ),
                gbranch(Label::text("decline"), end()),
            ],
        ),
    )
}
