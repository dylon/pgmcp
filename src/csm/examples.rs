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
