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

/// The **TapePaging** control protocol (Phase 6), with roles `O` = Orchestrator
/// (the paging *controller* / pi) and `Tape` = the context-tape data plane (the
/// `working_set_pages` residency state the [`PagingEngine`] mutates):
///
/// ```text
/// μ loop.
///   O → Tape : page_in_req .
///   Tape → O : page_in_ack .
///   O → Tape { get      : Tape → O : got      . loop
///            ; put      : Tape → O : put_ack  . loop
///            ; page_out : Tape → O : evicted  . loop
///            ; demote   : Tape → O : demoted  . loop
///            ; done     : end                        }
/// ```
///
/// [`PagingEngine`]: crate::tape::engine::PagingEngine
///
/// Each loop iteration is one paging *transaction*: the controller asks the data
/// plane to resolve + bring in a page set (`page_in_req`/`page_in_ack`), then
/// **drives the verb** via the sender-driven choice `O → Tape`. The five labels
/// are exactly the mechanical residency operations the engine performs against
/// the working set:
///
/// | Label | Engine operation ([`crate::tape::engine`] / [`crate::tape::store`]) |
/// |-------|----------------------------------------------------------------------|
/// | `get`      | a demand-hit / fetch of a resident page (`Tape → O : got`)        |
/// | `put`      | a write-back of a dirty page (`Tape → O : put_ack`)               |
/// | `page_out` | a budget-pressure eviction (`Tape → O : evicted`)                 |
/// | `demote`   | the demotion ladder pages in a summary (`Tape → O : demoted`)     |
/// | `done`     | the run completes; the working set is flushed and the loop ends   |
///
/// **Black-box-legal by construction.** Every edge is [`Label::text`] — there is
/// no latent/hidden-state hand-off — so the discipline gate
/// ([`crate::csm::media::check_media_discipline`]) admits the protocol for ANY
/// black-box role set (Claude Code, Codex). This is deliberate: paging must be
/// drivable and conformance-checkable by a black-box orchestrator, in contrast to
/// the white-box-only Tier-3 latent protocols. Residency itself remains a
/// MECHANICAL function of the budget + policy + logical clock — never an agent
/// judgment — so the protocol records *that* a verb happened, while the trust
/// boundary (which page) stays in the engine (mirroring the absent `Agent` arm in
/// [`crate::tracker::transition`]).
///
/// The controller (`O`) makes the choice, so `Tape` is the *receiver* of the
/// selection; both then continue as the chosen branch. The four non-terminal
/// arms re-enter `loop` (another paging transaction); `done` is the only exit.
pub fn tape_paging() -> GlobalType {
    /// One `verb : Tape → O : ack . loop` arm — the engine performs `verb`, the
    /// data plane acknowledges with `ack`, and the protocol loops for the next
    /// paging transaction.
    fn looping_arm(verb: &str, ack: &str) -> crate::csm::mpst::global::GlobalBranch {
        gbranch(
            Label::text(verb),
            interaction("Tape", "O", Label::text(ack), var("loop")),
        )
    }
    rec(
        "loop",
        interaction(
            "O",
            "Tape",
            Label::text("page_in_req"),
            interaction(
                "Tape",
                "O",
                Label::text("page_in_ack"),
                choice(
                    "O",
                    "Tape",
                    vec![
                        looping_arm("get", "got"),
                        looping_arm("put", "put_ack"),
                        looping_arm("page_out", "evicted"),
                        looping_arm("demote", "demoted"),
                        gbranch(Label::text("done"), end()),
                    ],
                ),
            ),
        ),
    )
}
