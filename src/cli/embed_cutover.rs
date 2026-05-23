//! `pgmcp embed-cutover` — operator CLI for the BGE-M3 migration.
//!
//! Default mode is `--check`: read-only status report (no mutations).
//! `--promote` flips `pgmcp_metadata.active_embedding_signature` to
//! `bge-m3-v1`; refuses unless the migration cron has fully drained
//! the backlog across all six tables (file_chunks, session_prompts,
//! git_commit_chunks, software_pattern_chunks, durable_mandates,
//! session_mandates). `--force` overrides the safety. `--to minilm`
//! is the rollback direction.
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phase 5 C9.

use std::path::Path;

use anyhow::Context;

use crate::config::Config;
use crate::cron::embedding_migration;
use crate::db;
use crate::embed;

/// Embed-cutover CLI mode. Parsed from the clap flags.
pub enum CutoverMode {
    /// Read-only status report (default).
    Check,
    /// Flip the active embedding signature to bge-m3-v1.
    PromoteToBgeM3 { force: bool },
    /// Roll back the active embedding signature to minilm-l6-v2.
    DemoteToMiniLm { force: bool },
    /// Drop the legacy 384d `embedding` columns + their HNSW indices.
    /// Refuses unless active sig is already `bge-m3-v1`.
    DropLegacy { force: bool },
}

/// Pretty-printed output format. `--json` switches to machine-readable
/// for scripted callers.
#[derive(Clone, Copy)]
pub enum OutputFormat {
    Text,
    Json,
}

pub async fn run(
    config_override: Option<&Path>,
    mode: CutoverMode,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let config = Config::load(config_override)?;
    crate::logging::init_cli_with_config(Some(&config));

    let pool = db::pool::create_pool(&config.database)
        .await
        .context("create db pool")?;

    match mode {
        CutoverMode::Check => check(&pool, &config, format).await,
        CutoverMode::PromoteToBgeM3 { force } => promote(&pool, &config, force, format).await,
        CutoverMode::DemoteToMiniLm { force } => demote(&pool, force, format).await,
        CutoverMode::DropLegacy { force } => drop_legacy(&pool, force, format).await,
    }
}

async fn check(pool: &sqlx::PgPool, config: &Config, format: OutputFormat) -> anyhow::Result<()> {
    let active = embedding_migration::active_embedding_signature(pool)
        .await
        .context("read active embedding signature")?;
    let bundled = embed::model::signature_for_model_name(&config.embeddings.model)
        .map(str::to_string)
        .unwrap_or_else(|e| format!("UNKNOWN ({e})"));
    let backlog = embedding_migration::full_backlog_counts(pool)
        .await
        .context("read backlog counts")?;
    let cron_interval = config.cron.embedding_migration_interval_secs;

    match format {
        OutputFormat::Json => {
            let body = serde_json::json!({
                "active_signature": active,
                "bundled_signature": bundled,
                "configured_model": config.embeddings.model,
                "cron_interval_secs": cron_interval,
                "cron_enabled": cron_interval > 0,
                "backlog": {
                    "file_chunks": backlog.file_chunks,
                    "session_prompts": backlog.session_prompts,
                    "git_commit_chunks": backlog.git_commit_chunks,
                    "software_pattern_chunks": backlog.software_pattern_chunks,
                    "durable_mandates": backlog.durable_mandates,
                    "session_mandates": backlog.session_mandates,
                    "total": backlog.total(),
                },
                "safe_to_promote": backlog.total() == 0 && bundled == "bge-m3-v1",
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&body).context("serialize")?
            );
        }
        OutputFormat::Text => {
            println!("Active signature (DB):       {active}");
            println!("Bundled signature (daemon):  {bundled}");
            println!("Configured daemon model:     {}", config.embeddings.model);
            println!(
                "Migration cron:              {}",
                if cron_interval > 0 {
                    format!("enabled (interval {}s)", cron_interval)
                } else {
                    "DISABLED (set [cron] embedding_migration_interval_secs > 0)".to_string()
                }
            );
            println!();
            println!("Backlog (rows with NULL embedding_v2 / embedding):");
            println!("  file_chunks:              {}", backlog.file_chunks);
            println!("  session_prompts:          {}", backlog.session_prompts);
            println!("  git_commit_chunks:        {}", backlog.git_commit_chunks);
            println!(
                "  software_pattern_chunks:  {}",
                backlog.software_pattern_chunks
            );
            println!("  durable_mandates:         {}", backlog.durable_mandates);
            println!("  session_mandates:         {}", backlog.session_mandates);
            println!("  ── total pending:         {}", backlog.total());
            println!();
            if backlog.total() == 0 && bundled == "bge-m3-v1" && active == "minilm-l6-v2" {
                println!("Status: SAFE to flip. Run `pgmcp embed-cutover --promote`.");
            } else if backlog.total() > 0 {
                println!(
                    "Status: NOT safe to flip. Wait for the cron to drain the backlog,\n\
                     or pass --force to bypass (NOT recommended)."
                );
            } else if active == "bge-m3-v1" && bundled == "bge-m3-v1" {
                println!(
                    "Status: cutover complete. After soak time, run \
                     `pgmcp embed-cutover --drop-legacy` to drop the legacy column."
                );
            } else {
                println!("Status: configuration unusual — investigate.");
            }
        }
    }
    Ok(())
}

async fn promote(
    pool: &sqlx::PgPool,
    config: &Config,
    force: bool,
    format: OutputFormat,
) -> anyhow::Result<()> {
    if !force {
        // Refuse if the daemon's bundled model isn't bge-m3 — promoting
        // the active sig to bge-m3-v1 while the daemon writes 384d
        // MiniLM would silently corrupt new chunks.
        let bundled = embed::model::signature_for_model_name(&config.embeddings.model)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if bundled != "bge-m3-v1" {
            anyhow::bail!(
                "pgmcp embed-cutover --promote refuses: configured daemon model is `{}` \
                 but you are promoting to bge-m3-v1. Set [embeddings] model = \"bge-m3\" \
                 first and restart the daemon, or pass --force to override (DANGEROUS).",
                config.embeddings.model
            );
        }
    }
    embedding_migration::promote_to_bge_m3(pool, force)
        .await
        .context("promote_to_bge_m3")?;
    match format {
        OutputFormat::Json => println!(
            "{}",
            serde_json::json!({"status": "ok", "new_signature": "bge-m3-v1"})
        ),
        OutputFormat::Text => {
            println!("✓ Cutover complete. active_embedding_signature = 'bge-m3-v1'.");
            println!("  Running daemons pick up the change within ~30 s (cache TTL).");
            println!(
                "  After a soak period, run `pgmcp embed-cutover --drop-legacy` to drop \
                 the legacy 384d columns and HNSW indices."
            );
        }
    }
    Ok(())
}

async fn demote(pool: &sqlx::PgPool, force: bool, format: OutputFormat) -> anyhow::Result<()> {
    // Demote requires every row to still have a legacy `embedding` value
    // (otherwise the daemon would read empty results post-rollback).
    if !force {
        let stranded: (i64,) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM file_chunks WHERE embedding IS NULL) \
             + (SELECT COUNT(*) FROM session_prompts WHERE embedding IS NULL)",
        )
        .fetch_one(pool)
        .await
        .context("count stranded rows")?;
        if stranded.0 > 0 {
            anyhow::bail!(
                "pgmcp embed-cutover --to minilm refuses: {} rows have NULL `embedding` \
                 (they were inserted after the daemon switched to BGE-M3). Rolling back \
                 now would make those rows invisible to legacy MiniLM readers. Pass \
                 --force to override (acceptable if you accept the data loss for rollback).",
                stranded.0
            );
        }
    }
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ('active_embedding_signature', $1)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind("minilm-l6-v2")
    .execute(pool)
    .await
    .context("write rollback signature")?;
    match format {
        OutputFormat::Json => println!(
            "{}",
            serde_json::json!({"status": "ok", "new_signature": "minilm-l6-v2"})
        ),
        OutputFormat::Text => println!(
            "✓ Rollback complete. active_embedding_signature = 'minilm-l6-v2'. \
             Restart the daemon with [embeddings] model = \"all-MiniLM-L6-v2\"."
        ),
    }
    Ok(())
}

async fn drop_legacy(pool: &sqlx::PgPool, force: bool, format: OutputFormat) -> anyhow::Result<()> {
    let active = embedding_migration::active_embedding_signature(pool)
        .await
        .context("read active signature")?;
    if !force && active != "bge-m3-v1" {
        anyhow::bail!(
            "pgmcp embed-cutover --drop-legacy refuses: active_embedding_signature is \
             `{active}`, not `bge-m3-v1`. Dropping the legacy column now would leave the \
             daemon with no readable embedding column. Promote first \
             (`pgmcp embed-cutover --promote`), then drop. Pass --force to override."
        );
    }
    // Drop legacy HNSW indices first (cheap if not present).
    let stmts = [
        "DROP INDEX IF EXISTS idx_file_chunks_embedding_hnsw",
        "DROP INDEX IF EXISTS idx_session_prompts_embedding_hnsw",
        "DROP INDEX IF EXISTS idx_git_commit_chunks_embedding_hnsw",
        "DROP INDEX IF EXISTS idx_software_pattern_chunks_embedding_hnsw",
        // Now drop the columns.
        "ALTER TABLE file_chunks DROP COLUMN IF EXISTS embedding",
        "ALTER TABLE session_prompts DROP COLUMN IF EXISTS embedding",
        "ALTER TABLE git_commit_chunks DROP COLUMN IF EXISTS embedding",
        "ALTER TABLE software_pattern_chunks DROP COLUMN IF EXISTS embedding",
    ];
    for s in stmts {
        sqlx::query(s)
            .execute(pool)
            .await
            .with_context(|| format!("exec: {s}"))?;
    }
    match format {
        OutputFormat::Json => println!(
            "{}",
            serde_json::json!({"status": "ok", "dropped": ["file_chunks.embedding",
                                                            "session_prompts.embedding",
                                                            "git_commit_chunks.embedding",
                                                            "software_pattern_chunks.embedding"]})
        ),
        OutputFormat::Text => println!(
            "✓ Dropped legacy 384d embedding columns and HNSW indices on \
             file_chunks, session_prompts, git_commit_chunks, software_pattern_chunks. \
             Re-run `pgmcp embed-cutover --check` to confirm."
        ),
    }
    Ok(())
}
