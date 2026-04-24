//! `stats` subcommand: print live counters from the running daemon.

use std::path::Path;

use crate::config::Config;
use crate::stats;

pub async fn run(config_override: Option<&Path>) -> anyhow::Result<()> {
    let config = Config::load(config_override)?;
    stats::cli::print_stats(&config).await?;
    Ok(())
}
