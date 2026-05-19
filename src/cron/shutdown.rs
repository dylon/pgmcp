//! Shared shutdown-awareness helpers for cron bodies.
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

use sqlx::Error;

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
}
