//! `reindex` subcommand: clear all indexed files + chunks + git history,
//! forcing the daemon to re-scan from scratch.

use std::path::Path;

use crate::config::Config;
use crate::db;

pub async fn run(config_override: Option<&Path>) -> anyhow::Result<()> {
    let config = Config::load(config_override)?;
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
