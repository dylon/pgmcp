//! The work-item status state machine: an explicit legal-transition matrix
//! with an **actor-capability gate**.
//!
//! `crate::daemon_state::DaemonPhase` enforces only *monotonic* order via
//! `fetch_max`, which cannot express `blocked ↔ in_progress` or
//! `rejected → in_progress`. So the tracker defines its own adjacency matrix
//! as a pure function. [`check_transition`] is the single chokepoint the
//! `set_status` query calls before any `UPDATE work_items SET status`, writing
//! the `work_item_status_history` row in the same transaction.
//!
//! The crux rules that make verification hard to game (see the plan §B/§G):
//!
//! 1. **`→ Verified` is `Gatekeeper`-only**, only from `ClaimedDone`/`Verifying`,
//!    and only with passing evidence. There is **no `Agent` arm into
//!    `Verified` anywhere in the matrix** — an agent cannot self-verify.
//! 2. **`→ Deferred` is `User`-only** and requires a `scope_negotiations`
//!    record. No `Agent` arm — an agent cannot self-defer.
//! 3. **`→ Rejected` is `Gatekeeper`-only** (driven by a failing evidence
//!    row) — an agent cannot mark its own work rejected to dodge
//!    re-verification.
//!
//! Everything an agent legitimately does (groom, start, block, claim done,
//! request verify, re-open) it can; everything that constitutes *judging its
//! own work* it cannot.

use serde::{Deserialize, Serialize};

use crate::tracker::status::WorkItemStatus;

/// Who is attempting a transition. `Gatekeeper` is the evidence-bearing path
/// (CI / Stop-hook / external auditor / experiment engine) — never the
/// authoring agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    User,
    Agent,
    Gatekeeper,
    System,
}

impl Actor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Gatekeeper => "gatekeeper",
            Self::System => "system",
        }
    }

    /// Parse an actor from its `as_str` form — the symmetric inverse of
    /// [`Actor::as_str`], kept for API completeness and round-trip use (the
    /// status-history `actor_kind` column is written via `as_str`). `serde`'s
    /// derived `Deserialize` covers the JSON path, so this string form has no
    /// internal caller yet; `#[allow(dead_code)]` documents that it is a
    /// deliberate part of the closed-enum surface, not an oversight.
    #[allow(dead_code)]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Self::User),
            "agent" => Some(Self::Agent),
            "gatekeeper" => Some(Self::Gatekeeper),
            "system" => Some(Self::System),
            _ => None,
        }
    }
}

/// Authorization context for a transition: what evidence / negotiation the
/// caller has on hand. Populated by the query layer from the DB
/// (`verification_evidence` / `scope_negotiations`) so the legality decision is
/// pure and testable.
#[derive(Debug, Clone, Copy, Default)]
pub struct TransitionContext {
    /// A `verification_evidence` row with `verdict = 'pass'` from a trusted
    /// source exists for every required criterion of the item.
    pub evidence_passing: bool,
    /// Any `verification_evidence` row (used for `→ Rejected`, a failing
    /// verdict).
    pub evidence_present: bool,
    /// A `scope_negotiations` row authorizes the deferral (user-only).
    pub user_negotiation: bool,
}

/// Why a transition was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionError {
    /// Source and target are the same status (no-op); callers may treat this
    /// as idempotent success rather than an error.
    NoOp { status: WorkItemStatus },
    /// No actor may perform this transition (not in the matrix).
    Illegal {
        from: WorkItemStatus,
        to: WorkItemStatus,
    },
    /// The transition is legal for some actor, but not this one.
    Unauthorized {
        from: WorkItemStatus,
        to: WorkItemStatus,
        actor: Actor,
    },
    /// `→ Verified` without a passing evidence row.
    EvidenceRequired { to: WorkItemStatus },
    /// `→ Rejected` without any evidence row.
    EvidenceMissingForRejection,
    /// `→ Deferred` without a user `scope_negotiations` record.
    NegotiationRequired,
}

impl TransitionError {
    /// Human-readable, agent-facing explanation (surfaced in tool errors).
    pub fn message(self) -> String {
        match self {
            Self::NoOp { status } => format!("item is already '{}'", status.as_str()),
            Self::Illegal { from, to } => format!(
                "no transition '{}' → '{}' exists",
                from.as_str(),
                to.as_str()
            ),
            Self::Unauthorized { from, to, actor } => format!(
                "actor '{}' may not transition '{}' → '{}'",
                actor.as_str(),
                from.as_str(),
                to.as_str()
            ),
            Self::EvidenceRequired { to } => format!(
                "'{}' is reached only by submitting passing acceptance evidence \
                 via record_evidence (agents cannot self-verify)",
                to.as_str()
            ),
            Self::EvidenceMissingForRejection => {
                "'rejected' requires a failing verification_evidence row".to_string()
            }
            Self::NegotiationRequired => {
                "'deferred' requires a user scope negotiation; agents cannot self-defer".to_string()
            }
        }
    }
}

use Actor::{Agent, Gatekeeper, System, User};
use WorkItemStatus::{
    Blocked, Cancelled, ClaimedDone, Deferred, InProgress, Pending, Ready, Rejected, Verified,
    Verifying,
};

// Reusable actor sets (kept as slices so the matrix reads like the plan's
// table). `UAS` = user/agent/system, `UA` = user/agent, `AU` = agent/user
// (claim), `U` = user-only, `G` = gatekeeper-only.
const UAS: &[Actor] = &[User, Agent, System];
const UA: &[Actor] = &[User, Agent];
const AU: &[Actor] = &[Agent, User];
const U: &[Actor] = &[User];
const G: &[Actor] = &[Gatekeeper];

/// The transition matrix. Returns the set of actors permitted to move
/// `from → to`; an empty slice means the transition does not exist. This is
/// the single, authoritative encoding of the plan's §B.3 adjacency table.
pub fn legal_actors(from: WorkItemStatus, to: WorkItemStatus) -> &'static [Actor] {
    match (from, to) {
        // pending
        (Pending, Ready) => UAS,
        (Pending, InProgress) => UA,
        (Pending, Blocked) => UA,
        (Pending, Deferred) => U,
        (Pending, Cancelled) => U,
        // ready
        (Ready, InProgress) => UA,
        (Ready, Blocked) => UAS,
        (Ready, Deferred) => U,
        (Ready, Cancelled) => U,
        // in_progress
        (InProgress, Blocked) => UAS,
        (InProgress, ClaimedDone) => AU,
        (InProgress, Verifying) => UA,
        (InProgress, Deferred) => U,
        (InProgress, Cancelled) => U,
        // blocked
        (Blocked, Ready) => UAS,
        (Blocked, InProgress) => UAS,
        (Blocked, Deferred) => U,
        (Blocked, Cancelled) => U,
        // claimed_done
        (ClaimedDone, InProgress) => UA,
        (ClaimedDone, Verifying) => UAS,
        (ClaimedDone, Verified) => G,
        (ClaimedDone, Rejected) => G,
        (ClaimedDone, Deferred) => U,
        (ClaimedDone, Cancelled) => U,
        // verifying
        (Verifying, InProgress) => UA,
        (Verifying, Verified) => G,
        (Verifying, Rejected) => G,
        (Verifying, Deferred) => U,
        (Verifying, Cancelled) => U,
        // verified (re-open allowed)
        (Verified, InProgress) => UA,
        (Verified, Cancelled) => U,
        // rejected (re-work)
        (Rejected, InProgress) => UA,
        (Rejected, Blocked) => UA,
        (Rejected, ClaimedDone) => AU,
        (Rejected, Verifying) => UA,
        (Rejected, Deferred) => U,
        (Rejected, Cancelled) => U,
        // deferred (reinstate is user-only)
        (Deferred, InProgress) => U,
        (Deferred, Cancelled) => U,
        // cancelled (re-open is user-only)
        (Cancelled, InProgress) => U,
        // everything else is forbidden
        _ => &[],
    }
}

/// Decide whether `actor` may move an item `from → to` given the available
/// evidence/negotiation context. Pure and total. The single chokepoint for
/// every status mutation.
pub fn check_transition(
    from: WorkItemStatus,
    to: WorkItemStatus,
    actor: Actor,
    ctx: TransitionContext,
) -> Result<(), TransitionError> {
    if from == to {
        return Err(TransitionError::NoOp { status: from });
    }
    let allowed = legal_actors(from, to);
    if allowed.is_empty() {
        return Err(TransitionError::Illegal { from, to });
    }
    if !allowed.contains(&actor) {
        return Err(TransitionError::Unauthorized { from, to, actor });
    }
    // Evidence / negotiation gates layered on top of actor capability.
    match to {
        Verified if !ctx.evidence_passing => Err(TransitionError::EvidenceRequired { to }),
        Rejected if !ctx.evidence_present => Err(TransitionError::EvidenceMissingForRejection),
        Deferred if !ctx.user_negotiation => Err(TransitionError::NegotiationRequired),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTORS: [Actor; 4] = [User, Agent, Gatekeeper, System];

    fn full_ctx() -> TransitionContext {
        TransitionContext {
            evidence_passing: true,
            evidence_present: true,
            user_negotiation: true,
        }
    }

    #[test]
    fn no_op_for_same_status() {
        for s in WorkItemStatus::ALL {
            assert!(matches!(
                check_transition(*s, *s, User, full_ctx()),
                Err(TransitionError::NoOp { .. })
            ));
        }
    }

    #[test]
    fn verified_is_gatekeeper_only_and_needs_evidence() {
        // No non-gatekeeper actor can ever reach Verified, from any state.
        for from in WorkItemStatus::ALL {
            for actor in ACTORS {
                let r = check_transition(*from, Verified, actor, full_ctx());
                if actor == Gatekeeper && matches!(*from, ClaimedDone | Verifying) {
                    assert!(r.is_ok(), "gatekeeper {from:?}->verified should pass");
                } else {
                    assert!(
                        r.is_err(),
                        "{actor:?} {from:?}->verified must be refused, got Ok"
                    );
                    // crucially, never a silent success for agents
                    if actor == Agent {
                        assert!(matches!(
                            r,
                            Err(TransitionError::Illegal { .. })
                                | Err(TransitionError::Unauthorized { .. })
                                | Err(TransitionError::NoOp { .. })
                        ));
                    }
                }
            }
        }
    }

    #[test]
    fn verified_blocked_without_passing_evidence() {
        let ctx = TransitionContext {
            evidence_passing: false,
            evidence_present: true,
            user_negotiation: true,
        };
        assert!(matches!(
            check_transition(ClaimedDone, Verified, Gatekeeper, ctx),
            Err(TransitionError::EvidenceRequired { .. })
        ));
        assert!(matches!(
            check_transition(Verifying, Verified, Gatekeeper, ctx),
            Err(TransitionError::EvidenceRequired { .. })
        ));
    }

    #[test]
    fn deferred_is_user_only_and_needs_negotiation() {
        for from in WorkItemStatus::ALL {
            for actor in ACTORS {
                let r = check_transition(*from, Deferred, actor, full_ctx());
                let legal = legal_actors(*from, Deferred);
                if legal.contains(&actor) {
                    // only User appears in any Deferred column
                    assert_eq!(actor, User, "only user may defer, {from:?} via {actor:?}");
                    assert!(r.is_ok());
                } else if *from != Deferred {
                    assert!(matches!(
                        r,
                        Err(TransitionError::Illegal { .. })
                            | Err(TransitionError::Unauthorized { .. })
                    ));
                }
            }
        }
        // user defer without a negotiation row is refused
        assert!(matches!(
            check_transition(
                InProgress,
                Deferred,
                User,
                TransitionContext {
                    user_negotiation: false,
                    ..full_ctx()
                }
            ),
            Err(TransitionError::NegotiationRequired)
        ));
        // agents have NO deferred arm anywhere
        for from in WorkItemStatus::ALL {
            assert!(!legal_actors(*from, Deferred).contains(&Agent));
        }
    }

    #[test]
    fn rejected_is_gatekeeper_only() {
        for from in WorkItemStatus::ALL {
            for actor in ACTORS {
                if legal_actors(*from, Rejected).contains(&actor) {
                    assert_eq!(actor, Gatekeeper, "only gatekeeper rejects, from {from:?}");
                }
            }
        }
        // entering rejected needs an evidence row
        assert!(matches!(
            check_transition(
                Verifying,
                Rejected,
                Gatekeeper,
                TransitionContext {
                    evidence_present: false,
                    ..full_ctx()
                }
            ),
            Err(TransitionError::EvidenceMissingForRejection)
        ));
    }

    #[test]
    fn matrix_is_irreflexive() {
        // No self-loops encoded in the matrix.
        for s in WorkItemStatus::ALL {
            assert!(legal_actors(*s, *s).is_empty(), "self-loop on {s:?}");
        }
    }

    #[test]
    fn happy_path_agent_flow_is_legal() {
        let c = full_ctx();
        assert!(check_transition(Pending, InProgress, Agent, c).is_ok());
        assert!(check_transition(InProgress, ClaimedDone, Agent, c).is_ok());
        assert!(check_transition(ClaimedDone, Verifying, Agent, c).is_ok());
        // but the agent hands off to the gatekeeper for the verdict
        assert!(check_transition(Verifying, Verified, Agent, c).is_err());
        assert!(check_transition(Verifying, Verified, Gatekeeper, c).is_ok());
    }
}
