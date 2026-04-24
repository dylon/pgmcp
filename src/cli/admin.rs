//! Admin subcommands: `init`, `upgrade-configs`, `init-project`, `upgrade-project`.
//!
//! Pure config-file operations — no DB access except `upgrade_configs` which
//! enumerates indexed projects to upgrade their `.pgmcp.toml` in bulk.

use std::path::{Path, PathBuf};

use crate::config::{self, Config};
use crate::db;

pub fn init() -> anyhow::Result<()> {
    let path = Config::write_default()?;
    println!("Default configuration written to: {}", path.display());
    Ok(())
}

pub async fn upgrade_configs(
    config_override: Option<&Path>,
    interactive: bool,
) -> anyhow::Result<()> {
    // Phase 1: Always upgrade global config
    let global_path = Config::upgrade(config_override)?;
    println!("Global configuration upgraded: {}", global_path.display());

    // Phase 2: Load freshly-upgraded config for DB connection
    let config = Config::load(config_override)?;

    // Phase 3: DB-driven project discovery + upgrade
    println!("Connecting to database for project discovery...");
    if let Err(e) = upgrade_all_project_configs(&config.database, interactive).await {
        eprintln!(
            "Warning: Could not upgrade project configs: {}\n\
             Use `pgmcp upgrade-project --cwd <DIR>` for individual projects.",
            e
        );
    }
    Ok(())
}

pub fn init_project(cwd: Option<PathBuf>) -> anyhow::Result<()> {
    let project_root =
        cwd.unwrap_or_else(|| std::env::current_dir().expect("Failed to get current directory"));
    let path = config::ProjectOverride::write_default(&project_root)?;
    println!("Project config written to: {}", path.display());
    Ok(())
}

pub fn upgrade_project(cwd: Option<PathBuf>) -> anyhow::Result<()> {
    let project_root =
        cwd.unwrap_or_else(|| std::env::current_dir().expect("Failed to get current directory"));
    let path = config::ProjectOverride::upgrade(&project_root)?;
    println!("Project config upgraded: {}", path.display());
    Ok(())
}

async fn upgrade_all_project_configs(
    db_config: &config::DatabaseConfig,
    interactive: bool,
) -> anyhow::Result<()> {
    let pool = db::pool::create_pool(db_config)
        .await
        .map_err(|e| anyhow::anyhow!("Database connection failed: {}", e))?;

    let projects = db::queries::list_projects(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to list projects: {}", e))?;

    if projects.is_empty() {
        println!("No indexed projects found.");
        return Ok(());
    }

    let mut upgraded = 0u32;
    let mut skipped = 0u32;
    let mut failed = 0u32;

    for project in &projects {
        let project_root = std::path::Path::new(&project.path);
        let pgmcp_toml = project_root.join(".pgmcp.toml");

        if !pgmcp_toml.exists() {
            skipped += 1;
            continue;
        }

        if interactive {
            eprint!(
                "Upgrade .pgmcp.toml in {} ({})? [y/N] ",
                project.name, project.path
            );
            use std::io::Write;
            std::io::stderr().flush()?;

            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            let answer = answer.trim().to_lowercase();
            if answer != "y" && answer != "yes" {
                println!("  Skipped {} (declined)", project.name);
                skipped += 1;
                continue;
            }
        }

        match config::ProjectOverride::upgrade(project_root) {
            Ok(path) => {
                println!("  Upgraded: {} ({})", project.name, path.display());
                upgraded += 1;
            }
            Err(e) => {
                eprintln!("  Failed: {} ({}): {}", project.name, project.path, e);
                failed += 1;
            }
        }
    }

    println!(
        "\nProject configs: {} upgraded, {} skipped, {} failed",
        upgraded, skipped, failed
    );
    Ok(())
}
