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
    crate::logging::init_cli_with_config(Some(&config));
    let pool = db::pool::create_pool(&config.database).await?;
    run_context_command(&pool, &config, cwd, depth).await
}

async fn run_context_command(
    pool: &sqlx::PgPool,
    config: &Config,
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

            let mandates = crate::mandates::resolve_effective_mandates(config, Some(&project));
            println!();
            print!("{}", crate::mandates::render_mandates_markdown(&mandates));

            // Phase 4: proactive digest (tracker / health / trend). Off unless
            // [digest] enabled = true. DB-only HEALTH (the CLI has no live
            // StatsTracker), project-scoped tracker + trend.
            emit_session_start_digest(pool, config, Some(project.id), &cwd_normalized).await;

            println!();
            println!("### Available pgmcp tools");
            println!(
                "Use ToolSearch to load: semantic_search, text_search, grep, read_file, list_projects, project_tree, mandate_context, file_info, index_stats, reindex, search_commits, software_pattern_search, recommend_design_patterns, review_design_patterns"
            );
            println!();
            println!(
                "**Tip:** Use mandate_context to reload effective AGENTS.md/CLAUDE.md/.pgmcp.toml context. Use search_commits for git history. Use semantic_search with project: \"claude\" or project: \"codex\" for past agent sessions/memory."
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
            let mandates = crate::mandates::resolve_effective_mandates(config, None);
            println!();
            print!("{}", crate::mandates::render_mandates_markdown(&mandates));

            // Phase 4: proactive digest, project-agnostic (no project resolved
            // for this cwd → cross-project HEALTH only; tracker/trend no-op
            // without a project scope). Off unless [digest] enabled = true.
            emit_session_start_digest(pool, config, None, &cwd_normalized).await;

            println!();
            println!("### Available pgmcp tools");
            println!(
                "Use ToolSearch to load: semantic_search, text_search, grep, read_file, list_projects, project_tree, mandate_context, file_info, index_stats, reindex, search_commits, software_pattern_search, recommend_design_patterns, review_design_patterns"
            );
            println!();
            println!(
                "**Tip:** Use mandate_context to reload effective AGENTS.md/CLAUDE.md/.pgmcp.toml context. Use search_commits for git history. Use semantic_search with project: \"claude\" or project: \"codex\" for past agent sessions/memory."
            );
        }
    }

    Ok(())
}

/// Compose and print the proactive digest on the SessionStart channel, gated by
/// `[digest] enabled && session_start`. The CLI has no live `StatsTracker` (so
/// HEALTH omits the cron-failure signal — `None`) and no client session id, so a
/// synthetic per-cwd key drives the [`maybe_emit`](crate::digest::maybe_emit)
/// dedup/rate-limit (re-running `pgmcp context` in the same directory within the
/// TTL won't re-print an identical digest). Best-effort: a DB error simply omits
/// the block rather than failing the context command.
async fn emit_session_start_digest(
    pool: &sqlx::PgPool,
    config: &Config,
    project_id: Option<i32>,
    cwd_normalized: &str,
) {
    let cfg = &config.digest;
    if !cfg.enabled || !cfg.session_start {
        return;
    }
    let digest = crate::digest::compose_digest(pool, project_id, None, cfg).await;
    if digest.is_empty() {
        return;
    }
    // Synthetic, stable per-cwd session key (the CLI has no client session id).
    let session_key = format!(
        "session-start:{}",
        crate::sessions::prompt_sha256(cwd_normalized)
    );
    if crate::digest::maybe_emit(
        pool,
        &session_key,
        crate::digest::DigestChannel::SessionStart,
        project_id,
        cfg,
        &digest,
    )
    .await
    {
        let block = digest.render_markdown(cfg.max_bytes);
        if !block.is_empty() {
            println!();
            print!("{block}");
        }
    }
}
