//! Administrative database operations driven by the daemon lifecycle, outside
//! the normal request/cron paths.
//!
//! Currently this is the graceful-shutdown sweep that terminates in-flight
//! heavy-cron backends so they release their table locks promptly. Heavy cron
//! transactions stamp themselves with `SET LOCAL application_name =
//! 'pgmcp:heavy:<job>'` (see the call sites that also raise `statement_timeout`);
//! [`terminate_heavy_backends`] targets exactly those.
//!
//! ## Why this exists
//!
//! A heavy analytic query (e.g. semantic-edges) runs server-side until it
//! finishes or hits `statement_timeout`. When the daemon shuts down it drops its
//! tokio runtime out from under such a query; the client socket closes, but with
//! `client_connection_check_interval` the *only* backstop, PostgreSQL may keep
//! executing the query — holding `ACCESS SHARE` on `indexed_files` /
//! `file_chunks` — for minutes after the process exited. The *next* daemon's
//! startup migrations need `ACCESS EXCLUSIVE` on those tables and would block on
//! the orphan, aborting startup with `canceling statement due to lock timeout`.
//!
//! Running this sweep during a graceful shutdown closes that window to ~0:
//! `pg_terminate_backend` makes the query error out immediately, releasing its
//! locks (and unblocking the cron worker blocked in `rt.block_on`). Ungraceful
//! death (SIGKILL / OOM / crash) cannot run the sweep; the
//! `client_connection_check_interval` GUC set in [`crate::db::pool`] is the
//! safety net there.

use sqlx::PgPool;

/// `application_name` prefix stamped on heavy cron transactions via
/// `SET LOCAL application_name = 'pgmcp:heavy:<job>'`. The shutdown sweep
/// targets exactly the backends whose `application_name` starts with this.
//
// Reached only from the daemon shutdown path (`cli::daemon` →
// `terminate_heavy_backends`), never from a `#[cfg(test)]` test, so the
// `bin pgmcp test` target (whose harness replaces `main`) flags it `dead_code`
// despite being live in the daemon; the lib build keeps it reachable via `pub`.
#[allow(dead_code)]
pub const HEAVY_APP_NAME_PREFIX: &str = "pgmcp:heavy:";

/// Terminate this database's *other* in-flight heavy-cron backends — those whose
/// `application_name` starts with [`HEAVY_APP_NAME_PREFIX`] — and return how many
/// were terminated.
///
/// No `state` filter: the heavy label is set with `SET LOCAL`, so it exists only
/// for the duration of the heavy transaction. Any backend carrying it is mid
/// heavy-transaction and is holding locks whether it is `active` (running its
/// query) or `idle in transaction` (between statements) — both must be reaped to
/// free the locks.
///
/// All pgmcp connections authenticate as the same role, so same-role
/// `pg_terminate_backend` needs no special privilege. The sweep excludes our own
/// backend (`pid <> pg_backend_pid()`) and is scoped to the current database.
#[allow(dead_code)] // live only in the daemon shutdown path; see HEAVY_APP_NAME_PREFIX note
pub async fn terminate_heavy_backends(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let terminated: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (
             SELECT pg_terminate_backend(pid) AS ok
             FROM pg_stat_activity
             WHERE datname = current_database()
               AND pid <> pg_backend_pid()
               AND application_name LIKE $1
         ) AS t
         WHERE t.ok",
    )
    .bind(format!("{HEAVY_APP_NAME_PREFIX}%"))
    .fetch_one(pool)
    .await?;
    Ok(terminated as u64)
}
