//! `statistics` subcommand (alias `stats`): print live counters from the
//! running daemon.
//!
//! Renamed from `stats` to avoid confusion with `status`. The shorter
//! alias is preserved for backward compatibility.

use std::path::Path;

use crate::config::Config;
use crate::stats;

pub async fn run(config_override: Option<&Path>) -> anyhow::Result<()> {
    crate::logging::init_cli();
    let config = Config::load(config_override)?;
    stats::cli::print_stats(&config).await?;
    Ok(())
}
