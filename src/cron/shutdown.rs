//! Shared shutdown-awareness + cron-error-classification helpers.
//!
//! ## Shutdown awareness
//!
//! Long-running cron loops (similarity scan, topic-clustering c-TF-IDF
//! streaming) march through batches and historically caught per-batch
//! DB errors with `error!(...); continue;`. That's correct for a one-
//! off transient failure but pathological at daemon shutdown: once
//! `db_pool.close()` runs, every subsequent batch hits the same
//! terminal error, so the loop spams thousands of identical log lines
//! before reaching its natural endpoint.
//!
//! `is_terminal_db_error` classifies the two variants we've observed
//! during shutdown so the affected loops can `break` instead of
//! `continue`:
//!
//! - `sqlx::Error::PoolClosed` — the direct case after `pool.close()`.
//! - "A Tokio 1.x context was found, but it is being shutdown" — the
//!   race-window case where the tokio runtime is draining before sqlx
//!   marks the pool closed.
//!
//! The string-matching fallback exists because the second variant
//! reaches us wrapped in a transport-level sqlx error, not as
//! `PoolClosed`. Display-text matching is a stable surface across
//! sqlx versions; the runtime-shutdown message is set by tokio itself.
//!
//! ## Cron error classification (Tier 4 / Followup 3)
//!
//! Beyond shutdown, cron loops historically had no way to distinguish
//! *transient* failures (one bad batch, retry next interval) from
//! *permanent* faults (`git` binary missing, persistent ENOSPC). The
//! `CronAction` enum and the `classify_*` helpers below give cron
//! bodies a uniform vocabulary:
//!
//! - `Continue` — log and try the next batch in the same run.
//! - `AbortRun` — break out of the current run, retry next tick.
//! - `Disable` — permanent fault for this job; the cron scheduler skips
//!   it on subsequent ticks until daemon restart.
//!
//! `StatsTracker::disable_cron_job(name)` records the disable; the
//! `CronStateMachine::execute_inline` skip-check reads it before
//! running. This stops log-spam loops on permanent faults without
//! requiring every cron body to invent its own sticky-flag scheme.

use sqlx::Error;

/// Recommended action when a cron body classifies an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronAction {
    /// Transient — log and proceed (next batch / next iteration).
    Continue,
    /// Bail out of the current run; the next scheduled tick will retry.
    AbortRun,
    /// Permanent fault for this job; the scheduler should skip it on
    /// future ticks. Callers pair this with `StatsTracker::disable_cron_job`.
    Disable,
}

/// True if this sqlx error means the cron should abort the entire scan
/// — the DB pool is closed or the tokio runtime is shutting down, and
/// every subsequent query will fail the same way. Worth a single
/// warn-and-bail rather than a per-batch cascade.
pub fn is_terminal_db_error(err: &Error) -> bool {
    if matches!(err, Error::PoolClosed) {
        return true;
    }
    let s = err.to_string();
    s.contains("attempted to acquire a connection on a closed pool")
        || s.contains("A Tokio 1.x context was found, but it is being shutdown")
}

/// Classify a `sqlx::Error` into the recommended cron action. Wraps
/// `is_terminal_db_error` so the cron body uses one consistent helper
/// shape across error kinds.
pub fn classify_db_error(err: &Error) -> CronAction {
    if is_terminal_db_error(err) {
        CronAction::AbortRun
    } else {
        CronAction::Continue
    }
}

/// Classify a `std::io::Error` into the recommended cron action. Used by
/// cron bodies that shell out (git, subprocess extraction) or touch the
/// filesystem (rescan walkers).
///
/// - `NotFound` / `PermissionDenied` on a critical resource (binary,
///   directory) → `Disable`. Retrying every hour cannot resolve these
///   without operator intervention.
/// - `StorageFull` / `OutOfMemory` → `AbortRun`. The condition may
///   clear on the next tick (cleanup, OOM-killer freeing pages).
/// - Anything else → `Continue`. Transient I/O hiccups.
pub fn classify_io_error(err: &std::io::Error) -> CronAction {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::NotFound | ErrorKind::PermissionDenied => CronAction::Disable,
        ErrorKind::StorageFull | ErrorKind::OutOfMemory => CronAction::AbortRun,
        _ => CronAction::Continue,
    }
}

/// Classify a git CLI stderr blob into the recommended cron action.
/// Used by `git_indexer` and downstream cron bodies that surface git's
/// own messages.
///
/// - "command not found" / "No such file" → `Disable` (`git` binary
///   missing). Permanent until reinstall.
/// - "not a git repository" / "does not exist" → `Disable` for the
///   affected project. The `.git/` directory check upstream usually
///   catches this earlier, but the classification is here for completeness.
/// - Anything else → `Continue`. Includes lock-file contention, network
///   blips during fetch, transient repo corruption.
pub fn classify_git_error(stderr: &str) -> CronAction {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("command not found")
        || lower.contains("no such file or directory")
        || lower.contains("not a git repository")
        || lower.contains("does not exist")
    {
        CronAction::Disable
    } else {
        CronAction::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_pool_closed_variant_as_terminal() {
        assert!(is_terminal_db_error(&Error::PoolClosed));
    }

    #[test]
    fn classifies_runtime_shutdown_message_as_terminal() {
        let e = Error::Protocol(
            "error communicating with database: A Tokio 1.x context was found, \
             but it is being shutdown."
                .into(),
        );
        assert!(is_terminal_db_error(&e));
    }

    #[test]
    fn classifies_closed_pool_string_as_terminal() {
        let e = Error::Protocol("attempted to acquire a connection on a closed pool".into());
        assert!(is_terminal_db_error(&e));
    }

    #[test]
    fn classifies_row_not_found_as_non_terminal() {
        assert!(!is_terminal_db_error(&Error::RowNotFound));
    }

    #[test]
    fn classifies_generic_protocol_error_as_non_terminal() {
        let e = Error::Protocol("malformed packet at byte 3".into());
        assert!(!is_terminal_db_error(&e));
    }

    #[test]
    fn classify_db_error_maps_terminal_to_abort_run() {
        assert_eq!(classify_db_error(&Error::PoolClosed), CronAction::AbortRun);
        assert_eq!(
            classify_db_error(&Error::Protocol("malformed".into())),
            CronAction::Continue,
        );
    }

    #[test]
    fn classify_io_error_maps_not_found_to_disable() {
        let err = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(classify_io_error(&err), CronAction::Disable);
        let perm = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert_eq!(classify_io_error(&perm), CronAction::Disable);
    }

    #[test]
    fn classify_io_error_maps_resource_pressure_to_abort_run() {
        let storage = std::io::Error::from(std::io::ErrorKind::StorageFull);
        assert_eq!(classify_io_error(&storage), CronAction::AbortRun);
        let oom = std::io::Error::from(std::io::ErrorKind::OutOfMemory);
        assert_eq!(classify_io_error(&oom), CronAction::AbortRun);
    }

    #[test]
    fn classify_io_error_maps_other_kinds_to_continue() {
        let interrupted = std::io::Error::from(std::io::ErrorKind::Interrupted);
        assert_eq!(classify_io_error(&interrupted), CronAction::Continue);
    }

    #[test]
    fn classify_git_error_detects_missing_binary() {
        assert_eq!(
            classify_git_error("/bin/sh: git: command not found"),
            CronAction::Disable
        );
        assert_eq!(
            classify_git_error("No such file or directory"),
            CronAction::Disable
        );
    }

    #[test]
    fn classify_git_error_detects_not_a_repo() {
        assert_eq!(
            classify_git_error(
                "fatal: not a git repository (or any of the parent directories): .git"
            ),
            CronAction::Disable
        );
    }

    #[test]
    fn classify_git_error_passes_transient_errors_through() {
        assert_eq!(
            classify_git_error(
                "fatal: unable to access 'https://github.com/x/y': SSL connect timeout"
            ),
            CronAction::Continue
        );
        assert_eq!(
            classify_git_error("fatal: Unable to create '.git/index.lock': File exists."),
            CronAction::Continue
        );
    }
}
