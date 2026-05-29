//! Completion roll-up — the pure aggregation half (the recursive-CTE data
//! gathering lives in `crate::db::queries::work_items::fetch_rollup_leaves`).
//!
//! Two numbers are produced and kept distinct on purpose:
//!
//! * `verified_fraction` — the **trustworthy** gate: weighted share of countable
//!   leaves that are `Verified` (and, for a `parametric` leaf, only when its
//!   universal-coverage criterion actually passed over the full corpus). A
//!   `ClaimedDone` leaf contributes **0** here — an agent's self-report never
//!   moves the trustworthy number.
//! * `claimed_fraction` — advisory: also counts `ClaimedDone` (what the agent
//!   *thinks* is done). Surfaced for UX, never used as a gate.
//!
//! `Deferred`/`Cancelled` leaves are excluded from the input entirely (the SQL
//! filters them), so they sit in neither the numerator nor the denominator — a
//! user-deferred subtree neither helps nor hurts its parent's completion.

use serde::Serialize;

use crate::tracker::status::WorkItemStatus;

/// One countable leaf's contribution inputs (deferred/cancelled already removed).
#[derive(Debug, Clone, Copy)]
pub struct LeafContribution {
    pub status: WorkItemStatus,
    /// Roll-up weight among siblings (clamped to ≥ 0 here).
    pub weight: f64,
    /// Whether this is a universal ("all/every/parity") clause.
    pub parametric: bool,
    /// For a parametric leaf: did a `universal` acceptance criterion pass over
    /// the full corpus (`coverage_count ≥ parametric_expected`)? Ignored for
    /// non-parametric leaves.
    pub universal_satisfied: bool,
}

/// Weighted completion of a subtree.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct RollupResult {
    /// Weighted fraction (0.0–1.0) of countable leaves that are verifiably done.
    pub verified_fraction: f64,
    /// Weighted fraction (0.0–1.0) incl. agent-claimed-but-unverified leaves.
    pub claimed_fraction: f64,
    /// Number of countable leaves (excludes deferred/cancelled).
    pub leaf_count: i64,
    pub verified_leaves: i64,
    pub claimed_leaves: i64,
}

impl LeafContribution {
    /// Is this leaf verifiably done? A parametric leaf needs full universal
    /// coverage; a normal leaf needs `Verified` status. (A parametric leaf with
    /// `status = Verified` but incomplete coverage is deliberately NOT done —
    /// the anti-single-case rule.)
    fn is_verified_done(&self) -> bool {
        if self.parametric {
            self.universal_satisfied
        } else {
            self.status.is_verified()
        }
    }

    /// Does the agent claim this leaf done (verified, or self-reported
    /// `ClaimedDone`)?
    fn is_claimed_done(&self) -> bool {
        self.is_verified_done() || matches!(self.status, WorkItemStatus::ClaimedDone)
    }
}

/// Aggregate leaf contributions into a weighted [`RollupResult`]. Pure; total.
/// An empty input (e.g. an all-deferred subtree) yields all-zero — nothing
/// countable means nothing verifiably done.
pub fn aggregate(leaves: &[LeafContribution]) -> RollupResult {
    let mut total_w = 0.0_f64;
    let mut verified_w = 0.0_f64;
    let mut claimed_w = 0.0_f64;
    let mut verified_n = 0_i64;
    let mut claimed_n = 0_i64;
    let mut counted_n = 0_i64;

    for leaf in leaves {
        // A `deferred`/`cancelled` leaf is excluded from both numerator and
        // denominator (it neither helps nor hurts). The SQL pre-filters these,
        // but applying the predicate here keeps `aggregate` correct on raw,
        // unfiltered input too.
        if leaf.status.is_excluded_from_rollup() {
            continue;
        }
        counted_n += 1;
        let w = leaf.weight.max(0.0);
        total_w += w;
        if leaf.is_verified_done() {
            verified_w += w;
            verified_n += 1;
        }
        if leaf.is_claimed_done() {
            claimed_w += w;
            claimed_n += 1;
        }
    }

    let frac = |num: f64| if total_w > 0.0 { num / total_w } else { 0.0 };
    RollupResult {
        verified_fraction: frac(verified_w),
        claimed_fraction: frac(claimed_w),
        leaf_count: counted_n,
        verified_leaves: verified_n,
        claimed_leaves: claimed_n,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::status::WorkItemStatus::*;

    fn leaf(status: crate::tracker::status::WorkItemStatus, weight: f64) -> LeafContribution {
        LeafContribution {
            status,
            weight,
            parametric: false,
            universal_satisfied: false,
        }
    }

    #[test]
    fn empty_subtree_is_zero() {
        let r = aggregate(&[]);
        assert_eq!(r.verified_fraction, 0.0);
        assert_eq!(r.claimed_fraction, 0.0);
        assert_eq!(r.leaf_count, 0);
    }

    #[test]
    fn single_verified_is_full() {
        let r = aggregate(&[leaf(Verified, 1.0)]);
        assert_eq!(r.verified_fraction, 1.0);
        assert_eq!(r.claimed_fraction, 1.0);
    }

    #[test]
    fn claimed_done_does_not_count_as_verified() {
        let r = aggregate(&[leaf(ClaimedDone, 1.0)]);
        assert_eq!(
            r.verified_fraction, 0.0,
            "claimed_done must NOT be verified"
        );
        assert_eq!(r.claimed_fraction, 1.0, "but it does count as claimed");
    }

    #[test]
    fn mixed_verified_and_claimed_is_weighted() {
        // two verified + one claimed_done, equal weights.
        let r = aggregate(&[
            leaf(Verified, 1.0),
            leaf(Verified, 1.0),
            leaf(ClaimedDone, 1.0),
        ]);
        assert!((r.verified_fraction - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(r.claimed_fraction, 1.0);
        assert_eq!(r.verified_leaves, 2);
        assert_eq!(r.claimed_leaves, 3);
    }

    #[test]
    fn weighting_favors_heavier_leaves() {
        // a verified weight-3 leaf and a pending weight-1 leaf ⇒ 3/4 verified.
        let r = aggregate(&[leaf(Verified, 3.0), leaf(Pending, 1.0)]);
        assert!((r.verified_fraction - 0.75).abs() < 1e-9);
    }

    #[test]
    fn deferred_and_cancelled_leaves_are_excluded() {
        // A deferred/cancelled leaf is excluded from numerator AND denominator:
        // one verified + one deferred + one cancelled ⇒ 1/1 verified, count 1.
        let r = aggregate(&[
            leaf(Verified, 1.0),
            leaf(Deferred, 5.0),
            leaf(Cancelled, 5.0),
        ]);
        assert_eq!(r.verified_fraction, 1.0, "deferred/cancelled don't dilute");
        assert_eq!(r.leaf_count, 1, "only the verified leaf is countable");
        assert_eq!(r.verified_leaves, 1);
        // An all-excluded subtree is all-zero (nothing countable).
        let z = aggregate(&[leaf(Deferred, 1.0), leaf(Cancelled, 1.0)]);
        assert_eq!(z.verified_fraction, 0.0);
        assert_eq!(z.leaf_count, 0);
    }

    #[test]
    fn triage_and_confirmed_dilute_like_pending() {
        // An open bug (triage or confirmed) is countable but not verified — it
        // dilutes a parent's verified_fraction exactly like `pending`, and is
        // NOT excluded the way deferred/cancelled are.
        assert!(!Triage.is_excluded_from_rollup());
        assert!(!Confirmed.is_excluded_from_rollup());
        let r = aggregate(&[leaf(Verified, 1.0), leaf(Triage, 1.0), leaf(Confirmed, 1.0)]);
        assert!(
            (r.verified_fraction - 1.0 / 3.0).abs() < 1e-9,
            "triage/confirmed dilute the verified fraction"
        );
        assert_eq!(r.leaf_count, 3, "all three are countable");
        assert_eq!(r.verified_leaves, 1);
        assert_eq!(
            r.claimed_leaves, 1,
            "triage/confirmed are not even agent-claimed"
        );
    }

    #[test]
    fn parametric_needs_universal_coverage_not_status() {
        // A parametric leaf whose status is Verified but coverage incomplete is
        // NOT done (anti-single-case).
        let not_covered = LeafContribution {
            status: Verified,
            weight: 1.0,
            parametric: true,
            universal_satisfied: false,
        };
        assert_eq!(aggregate(&[not_covered]).verified_fraction, 0.0);

        let covered = LeafContribution {
            status: InProgress, // status irrelevant once parametric coverage is met
            weight: 1.0,
            parametric: true,
            universal_satisfied: true,
        };
        assert_eq!(aggregate(&[covered]).verified_fraction, 1.0);
    }
}
