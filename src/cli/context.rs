//! `context` subcommand: print project context for the current working
//! directory (used by the Claude Code SessionStart hook).

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::db;

pub async fn run(
    config_override: Option<&Path>,
    cwd: Option<PathBuf>,
    depth: i32,
) -> anyhow::Result<()> {
    let config = Config::load(config_override)?;
    let pool = db::pool::create_pool(&config.database).await?;
    run_context_command(&pool, cwd, depth).await
}

async fn run_context_command(
    pool: &sqlx::PgPool,
    cwd: Option<PathBuf>,
    depth: i32,
) -> anyhow::Result<()> {
    let cwd_str = match cwd {
        Some(p) => p.to_string_lossy().into_owned(),
        None => std::env::current_dir()?.to_string_lossy().into_owned(),
    };

    // Ensure trailing slash for prefix matching
    let cwd_normalized = if cwd_str.ends_with('/') {
        cwd_str.clone()
    } else {
        format!("{}/", cwd_str)
    };

    match db::queries::find_project_by_cwd(pool, &cwd_normalized).await? {
        Some(project) => {
            let file_count = project.file_count.unwrap_or(0);
            let last_scanned = project
                .last_scanned_at
                .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                .unwrap_or_else(|| "never".into());

            println!("## pgmcp: Project Context for \"{}\"", project.name);
            println!();
            println!(
                "**Root:** {}  |  **Files indexed:** {}  |  **Last scanned:** {}",
                project.path, file_count, last_scanned
            );

            // Language breakdown
            let languages = db::queries::language_summary(pool, &project.name).await?;
            if !languages.is_empty() {
                println!();
                println!("### Languages");
                for lang in &languages {
                    println!("- {}: {} files", lang.language, lang.count);
                }
            }

            // File tree
            let tree = db::queries::project_tree(pool, &project.name, depth).await?;
            if !tree.is_empty() {
                println!();
                println!("### File Tree (depth {})", depth);
                for path in &tree {
                    println!("{}", path);
                }
            }

            println!();
            println!("### Available pgmcp tools");
            println!(
                "Use ToolSearch to load: semantic_search, text_search, grep, read_file, list_projects, project_tree, file_info, index_stats, reindex, search_commits"
            );
            println!();
            println!(
                "**Tip:** Use search_commits for git history. Use semantic_search with project: \"claude\" for past Claude Code sessions/memory."
            );
        }
        None => {
            println!("## pgmcp: No indexed project found for {}", cwd_str);
            println!();
            let projects = db::queries::list_projects(pool).await?;
            if projects.is_empty() {
                println!("No projects are currently indexed.");
            } else {
                println!("### Indexed projects");
                for p in &projects {
                    println!(
                        "- **{}** ({}, {} files)",
                        p.name,
                        p.path,
                        p.file_count.unwrap_or(0)
                    );
                }
            }
            println!();
            println!("### Available pgmcp tools");
            println!(
                "Use ToolSearch to load: semantic_search, text_search, grep, read_file, list_projects, project_tree, file_info, index_stats, reindex, search_commits"
            );
            println!();
            println!(
                "**Tip:** Use search_commits for git history. Use semantic_search with project: \"claude\" for past Claude Code sessions/memory."
            );
        }
    }

    Ok(())
}
