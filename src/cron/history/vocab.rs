//! Closed vocabularies for the `cron_run_history` ledger (v40). Per the ADR-003
//! idiom each is a `TEXT` column + a `CHECK` built from a closed Rust enum via
//! [`sql_in_list`], with a `#[cfg(test)]` golden test pinning the vocabulary —
//! the same idiom as [`crate::tracker::severity`] and [`crate::tracker::kind`].
//!
//! [`CronOutcome`] is a **persistence-layer** enum, deliberately distinct from
//! the in-memory [`crate::stats::tracker::CronJobOutcome`] (whose `as_str()`
//! flattens `Skipped(reason)` into `"skipped:<reason>"` for the existing
//! `index_stats` JSON snapshot + its goldens). The persistence layer stores the
//! reason in a separate `skip_reason` column and adds a `Failed` variant that
//! the in-memory enum does not have (an internal error caught by a cron body's
//! top-level `Err` arm, distinct from a `Panicked` unwind).

use crate::stats::tracker::{CronJobOutcome, SkipReason};
use crate::tracker::kind::join_quoted;

/// How a cron run was initiated. Stored in `cron_run_history.trigger_source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CronTriggerSource {
    /// A recurring tick from the in-process scheduler.
    Scheduled,
    /// An operator/agent invocation via the `trigger_cron` MCP tool.
    Manual,
    /// A one-shot run scheduled at daemon startup (restart-survival path).
    Startup,
}

impl CronTriggerSource {
    /// Canonical list; the source of the DB CHECK vocabulary.
    pub const ALL: &'static [CronTriggerSource] = &[Self::Scheduled, Self::Manual, Self::Startup];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Manual => "manual",
            Self::Startup => "startup",
        }
    }

    #[allow(dead_code)]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.as_str() == s)
    }
}

/// SQL `IN (...)` value list built from [`CronTriggerSource::ALL`] — the single
/// source of truth shared with the `cron_run_history_trigger_source_check`
/// constraint.
pub fn trigger_source_sql_in_list() -> String {
    join_quoted(CronTriggerSource::ALL.iter().map(|t| t.as_str()))
}

/// The terminal outcome of a cron run, as persisted in `cron_run_history.outcome`.
///
/// - `Ok`: the body completed normally.
/// - `NoOp`: the body's empty-data path returned immediately (nothing to do).
/// - `Skipped`: a gate short-circuited the run before the body; the *reason*
///   lives in the separate `skip_reason` column (see [`SkipReason`]).
/// - `Panicked`: the body unwound through `catch_unwind` (or no finisher ran).
/// - `Failed`: the body returned an internal error from its top-level `Err`
///   arm; the message lives in `error_detail`. (No in-memory analogue.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CronOutcome {
    Ok,
    NoOp,
    Skipped,
    Panicked,
    Failed,
}

impl CronOutcome {
    /// Canonical list; the source of the DB CHECK vocabulary.
    pub const ALL: &'static [CronOutcome] = &[
        Self::Ok,
        Self::NoOp,
        Self::Skipped,
        Self::Panicked,
        Self::Failed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NoOp => "noop",
            Self::Skipped => "skipped",
            Self::Panicked => "panicked",
            Self::Failed => "failed",
        }
    }

    #[allow(dead_code)]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|o| o.as_str() == s)
    }
}

/// SQL `IN (...)` value list built from [`CronOutcome::ALL`] — the single source
/// of truth shared with the `cron_run_history_outcome_check` constraint.
pub fn outcome_sql_in_list() -> String {
    join_quoted(CronOutcome::ALL.iter().map(|o| o.as_str()))
}

/// Map the in-memory [`CronJobOutcome`] (recorded by the scheduler today) onto
/// the persistence pair `(CronOutcome, Option<SkipReason>)`. Note the in-memory
/// enum has no `Failed` variant — that is only ever produced directly by
/// `CronRunGuard::fail`.
impl From<CronJobOutcome> for (CronOutcome, Option<SkipReason>) {
    fn from(o: CronJobOutcome) -> Self {
        match o {
            CronJobOutcome::Ok => (CronOutcome::Ok, None),
            CronJobOutcome::NoOp => (CronOutcome::NoOp, None),
            CronJobOutcome::Skipped(r) => (CronOutcome::Skipped, Some(r)),
            CronJobOutcome::Panicked => (CronOutcome::Panicked, None),
        }
    }
}

/// The six [`SkipReason`] variants, in canonical order — the source of the
/// `cron_run_history_skip_reason_check` vocabulary. `SkipReason` lives in
/// `src/stats/tracker/outcomes.rs` and intentionally exposes no `ALL` constant
/// (its goldens are pinned there); we enumerate it here, guarded by the
/// compile-time exhaustiveness check below so a new variant cannot silently
/// drift from the CHECK.
const SKIP_REASONS: &[SkipReason] = &[
    SkipReason::PhaseGate,
    SkipReason::Cooldown,
    SkipReason::LockBusy,
    SkipReason::Shutdown,
    SkipReason::DbDown,
    SkipReason::DiskPressure,
];

/// Compile-time exhaustiveness lock: adding a `SkipReason` variant breaks this
/// wildcard-free match, forcing [`SKIP_REASONS`] (and the v40 CHECK) to be
/// updated in lockstep.
#[allow(dead_code)]
fn skip_reason_exhaustiveness(r: SkipReason) {
    match r {
        SkipReason::PhaseGate
        | SkipReason::Cooldown
        | SkipReason::LockBusy
        | SkipReason::Shutdown
        | SkipReason::DbDown
        | SkipReason::DiskPressure => {}
    }
}

/// SQL `IN (...)` value list built from the six [`SkipReason`] variants — the
/// single source of truth shared with the `cron_run_history_skip_reason_check`
/// constraint.
pub fn skip_reason_sql_in_list() -> String {
    join_quoted(SKIP_REASONS.iter().map(|r| r.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn trigger_source_vocabulary_is_pinned() {
        let got: HashSet<&str> = CronTriggerSource::ALL.iter().map(|t| t.as_str()).collect();
        let expected: HashSet<&str> = ["scheduled", "manual", "startup"].into_iter().collect();
        assert_eq!(got, expected, "CronTriggerSource vocabulary drifted");
        assert_eq!(CronTriggerSource::ALL.len(), 3);
        assert_eq!(got.len(), 3, "duplicate as_str() in CronTriggerSource");
    }

    #[test]
    fn outcome_vocabulary_is_pinned() {
        let got: HashSet<&str> = CronOutcome::ALL.iter().map(|o| o.as_str()).collect();
        let expected: HashSet<&str> = ["ok", "noop", "skipped", "panicked", "failed"]
            .into_iter()
            .collect();
        assert_eq!(got, expected, "CronOutcome vocabulary drifted");
        assert_eq!(CronOutcome::ALL.len(), 5);
        assert_eq!(got.len(), 5, "duplicate as_str() in CronOutcome");
    }

    #[test]
    fn parse_roundtrips_for_all() {
        for t in CronTriggerSource::ALL {
            assert_eq!(CronTriggerSource::parse(t.as_str()), Some(*t));
        }
        assert_eq!(CronTriggerSource::parse("nonsense"), None);
        for o in CronOutcome::ALL {
            assert_eq!(CronOutcome::parse(o.as_str()), Some(*o));
        }
        assert_eq!(CronOutcome::parse("nonsense"), None);
    }

    #[test]
    fn sql_in_lists_quote_every_value() {
        let t = trigger_source_sql_in_list();
        assert!(t.starts_with("'scheduled'"), "got: {t}");
        assert_eq!(t.matches('\'').count(), CronTriggerSource::ALL.len() * 2);
        assert_eq!(t.matches(',').count(), CronTriggerSource::ALL.len() - 1);

        let o = outcome_sql_in_list();
        assert!(o.contains("'failed'"));
        assert_eq!(o.matches('\'').count(), CronOutcome::ALL.len() * 2);
        assert_eq!(o.matches(',').count(), CronOutcome::ALL.len() - 1);
    }

    #[test]
    fn skip_reason_list_matches_the_six_variants() {
        let got: HashSet<&str> = SKIP_REASONS.iter().map(|r| r.as_str()).collect();
        let expected: HashSet<&str> = [
            "phase_gate",
            "cooldown",
            "lock_busy",
            "shutdown",
            "db_down",
            "disk_pressure",
        ]
        .into_iter()
        .collect();
        assert_eq!(got, expected, "SkipReason list drifted from outcomes.rs");
        assert_eq!(SKIP_REASONS.len(), 6);
        let s = skip_reason_sql_in_list();
        assert_eq!(s.matches('\'').count(), 6 * 2);
        assert_eq!(s.matches(',').count(), 5);
    }

    #[test]
    fn from_in_memory_outcome_maps_cleanly() {
        assert_eq!(
            <(CronOutcome, Option<SkipReason>)>::from(CronJobOutcome::Ok),
            (CronOutcome::Ok, None)
        );
        assert_eq!(
            <(CronOutcome, Option<SkipReason>)>::from(CronJobOutcome::NoOp),
            (CronOutcome::NoOp, None)
        );
        assert_eq!(
            <(CronOutcome, Option<SkipReason>)>::from(CronJobOutcome::Panicked),
            (CronOutcome::Panicked, None)
        );
        assert_eq!(
            <(CronOutcome, Option<SkipReason>)>::from(CronJobOutcome::Skipped(
                SkipReason::Cooldown
            )),
            (CronOutcome::Skipped, Some(SkipReason::Cooldown))
        );
    }
}
