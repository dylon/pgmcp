//! The PURE policy that maps "a commit/PR referenced this work item" to the
//! status the **agent-grade** auto-transition should advance it to — and,
//! critically, the place the Phase-3 trust invariant is pinned.
//!
//! A commit or merge is an *agent-grade* signal: the indexer / `pr_event`
//! handler runs the resulting transition as
//! [`crate::tracker::transition::Actor::Agent`]. The matrix
//! ([`crate::tracker::transition::legal_actors`]) has **no `Agent` arm into
//! `Verified`/`Rejected`/`Deferred`/`Confirmed`** — those are the
//! gatekeeper/user judgment columns — so by construction this function can never
//! return one of them. The exhaustive [`tests::never_returns_a_judgment_status`]
//! test makes that structural: it asserts, over `WorkItemStatus::ALL ×
//! {closing, non-closing}`, that the result is never a judgment status and is
//! always an `Agent`-legal target from its source. If a future edit tried to
//! advance an item to `verified` on a commit, this test fails before it ships.
//!
//! Policy:
//!   - a **bare reference** (`#<public_id>`, no closing verb) is a *touch*: a
//!     not-yet-started item (`Pending`/`Confirmed`/`Ready`/`Blocked`) advances
//!     to `InProgress`; anything already in flight or terminal is left alone.
//!   - a **closing reference** (`fixes`/`closes`/`resolves`/… `<public_id>`)
//!     additionally promotes an `InProgress` item to `ClaimedDone` (a *claim*,
//!     NOT a verification — the evidence gate still stands). A closing verb on a
//!     not-yet-started item still only advances it to `InProgress` (the matrix
//!     has no direct jump to `ClaimedDone` from those states), so the worst a
//!     mislabelled commit can do is start an item early.

use crate::tracker::status::WorkItemStatus;

/// The status an `Actor::Agent` auto-transition should move a referenced item
/// to, or `None` for a no-op (already in flight with a bare ref, or terminal).
///
/// `is_closing` is whether the reference used a closing verb (see
/// [`crate::tracker::commit_ref::is_closing_ref`]).
///
/// Every `Some(target)` returned here is an `Agent`-legal transition from
/// `from` in [`crate::tracker::transition::legal_actors`]; never a judgment
/// status (`Verified`/`Rejected`/`Deferred`/`Confirmed`). The caller still runs
/// the result through `set_work_item_status` → `check_transition`, so even a
/// policy bug cannot bypass the chokepoint — this function only ever *narrows*
/// what the agent path may attempt.
pub fn next_auto_status(from: WorkItemStatus, is_closing: bool) -> Option<WorkItemStatus> {
    use WorkItemStatus::*;
    match (from, is_closing) {
        // A closing reference on an in-flight item is a CLAIM (not a verify):
        // in_progress → claimed_done (Agent-legal via the `AU` set). The
        // evidence-backed → verified gate is untouched.
        (InProgress, true) => Some(ClaimedDone),
        // A bare reference on an in-flight item is a no-op (it is already
        // started; we do not regress or re-claim it).
        (InProgress, false) => None,
        // Not-yet-started states advance to in_progress on ANY reference
        // (bare or closing) — a closing verb cannot jump them straight to
        // claimed_done because the matrix has no such arc, so the strongest
        // safe move is "start it".
        (Pending | Confirmed | Ready | Blocked, _) => Some(InProgress),
        // Already claimed/verifying/verified/rejected/deferred/cancelled, or
        // the bug-intake `Triage` state (whose only exit is the user-only
        // `→ confirmed`): a commit reference does not move them. In particular
        // `Triage` is deliberately excluded so a commit cannot bypass the
        // user-only confirmation gate.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::transition::{Actor, legal_actors};

    /// The judgment columns an agent-grade signal must NEVER reach.
    const JUDGMENT: [WorkItemStatus; 4] = [
        WorkItemStatus::Verified,
        WorkItemStatus::Rejected,
        WorkItemStatus::Deferred,
        WorkItemStatus::Confirmed,
    ];

    #[test]
    fn never_returns_a_judgment_status() {
        // EXHAUSTIVE over every (from, is_closing) pair: the auto-transition is
        // structurally incapable of fabricating a verification / rejection /
        // deferral / confirmation, and every move it DOES propose is legal for
        // Actor::Agent from that source. This is the Phase-3 trust pin.
        for &from in WorkItemStatus::ALL {
            for is_closing in [true, false] {
                let Some(to) = next_auto_status(from, is_closing) else {
                    continue;
                };
                assert!(
                    !JUDGMENT.contains(&to),
                    "next_auto_status({from:?}, closing={is_closing}) = {to:?} — \
                     a judgment status an agent must never auto-reach"
                );
                assert!(
                    legal_actors(from, to).contains(&Actor::Agent),
                    "next_auto_status({from:?}, closing={is_closing}) = {to:?} is NOT an \
                     Agent-legal transition in the matrix — the agent path could not perform it"
                );
            }
        }
    }

    #[test]
    fn bare_ref_starts_not_yet_started_items() {
        use WorkItemStatus::*;
        for from in [Pending, Confirmed, Ready, Blocked] {
            assert_eq!(
                next_auto_status(from, false),
                Some(InProgress),
                "a bare reference starts {from:?}"
            );
        }
    }

    #[test]
    fn closing_ref_claims_in_progress() {
        assert_eq!(
            next_auto_status(WorkItemStatus::InProgress, true),
            Some(WorkItemStatus::ClaimedDone),
            "a closing reference claims an in_progress item (NOT verify)"
        );
    }

    #[test]
    fn bare_ref_on_in_progress_is_noop() {
        assert_eq!(next_auto_status(WorkItemStatus::InProgress, false), None);
    }

    #[test]
    fn closing_ref_on_not_started_only_starts() {
        // A closing verb cannot jump a pending item straight to claimed_done;
        // the strongest safe move is in_progress.
        use WorkItemStatus::*;
        for from in [Pending, Confirmed, Ready, Blocked] {
            assert_eq!(next_auto_status(from, true), Some(InProgress));
        }
    }

    #[test]
    fn terminal_and_in_review_states_are_noops() {
        use WorkItemStatus::*;
        for from in [
            ClaimedDone,
            Verifying,
            Verified,
            Rejected,
            Deferred,
            Cancelled,
        ] {
            assert_eq!(next_auto_status(from, false), None, "{from:?} bare");
            assert_eq!(next_auto_status(from, true), None, "{from:?} closing");
        }
    }

    #[test]
    fn triage_is_never_auto_advanced() {
        // The bug-intake state's only exit is the user-only → confirmed; a
        // commit reference must not bypass that gate.
        assert_eq!(next_auto_status(WorkItemStatus::Triage, false), None);
        assert_eq!(next_auto_status(WorkItemStatus::Triage, true), None);
    }
}
