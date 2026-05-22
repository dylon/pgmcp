//! `reindex` subcommand: clear all indexed files + chunks + git history,
//! forcing the daemon to re-scan from scratch.
//!
//! Refuses to run if the daemon is alive (its inference workers may
//! have rows mid-pipeline; deleting them out from under the worker
//! produces FK-violation cascades). Use `--force` to override when you
//! are certain the daemon is stopped but the port appears listening
//! (e.g. socket lingering, kernel cleanup pending).

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

use crate::config::Config;
use crate::db;

const DAEMON_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

pub async fn run(config_override: Option<&Path>, force: bool) -> anyhow::Result<()> {
    let config = Config::load(config_override)?;
    crate::logging::init_cli_with_config(Some(&config));

    if !force && let Some(addr) = daemon_listening(&config) {
        anyhow::bail!(
            "pgmcp reindex: daemon is running on {addr}.\n\
             Stop the daemon first (e.g. `pkill -TERM -f 'pgmcp daemon'`),\n\
             then re-run `pgmcp reindex`, then restart the daemon.\n\
             (Pass --force to bypass this check; use only when the daemon\n\
             is verified stopped and the listening socket is just lingering.)"
        );
    }

    println!("Triggering full re-index of all workspaces...");
    let pool = db::pool::create_pool(&config.database).await?;
    db::migrations::run_migrations(&pool, &config.vector).await?;
    sqlx::query("DELETE FROM git_commit_chunks")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM git_commits")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM file_chunks")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM indexed_files")
        .execute(&pool)
        .await?;
    // Clear git last commit markers
    sqlx::query("DELETE FROM pgmcp_metadata WHERE key LIKE 'git_last_commit:%'")
        .execute(&pool)
        .await?;
    println!("Index cleared (files + git history). Restart pgmcp to re-index.");
    Ok(())
}

/// Return `Some(addr)` if a TCP connect to `config.mcp.host:port` succeeds
/// within `DAEMON_PROBE_TIMEOUT`, indicating a daemon is listening.
///
/// `None` covers the no-listener cases (connection refused, host down,
/// DNS failure, timeout). False negatives are conservatively safe — we
/// proceed with the destructive `DELETE` only when we're confident
/// nothing answers on the configured port.
fn daemon_listening(config: &Config) -> Option<SocketAddr> {
    let host_port = format!("{}:{}", config.mcp.host, config.mcp.port);
    let addr = host_port.to_socket_addrs().ok()?.next()?;
    match TcpStream::connect_timeout(&addr, DAEMON_PROBE_TIMEOUT) {
        Ok(_) => Some(addr),
        Err(_) => None,
    }
}
