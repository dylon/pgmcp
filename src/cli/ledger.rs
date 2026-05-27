//! `pgmcp ledger` — render experiment ledgers and inspect their frontmatter.
//!
//! `render` writes (or, with `--dry-run`, returns) the markdown ledger for an
//! experiment via the `experiment_render_ledger` tool. `import` parses the
//! YAML frontmatter of existing ledger files to show the `pgmcp_experiment`
//! slug join-key — the structured DB record remains the source of truth, so
//! lossy structured backfill of legacy prose ledgers is intentionally not
//! performed (they stay searchable as indexed prose).

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::Subcommand;

use crate::config::Config;
use crate::indexer::frontmatter;

#[derive(Subcommand, Debug)]
pub enum LedgerCmd {
    /// Render an experiment's record to docs/scientific-ledger/<slug>-<date>.md.
    Render {
        /// Experiment id (or use --slug).
        #[arg(long)]
        experiment: Option<i64>,
        /// Experiment slug (or use --experiment).
        #[arg(long)]
        slug: Option<String>,
        /// Render and print without writing the file.
        #[arg(long)]
        dry_run: bool,
    },
    /// Inspect rendered ledger frontmatter (the slug join-key back to the DB).
    Import {
        /// Ledger markdown files to inspect.
        files: Vec<PathBuf>,
    },
}

pub async fn run(config_override: Option<&Path>, sub: LedgerCmd) -> anyhow::Result<()> {
    let config = Config::load(config_override)?;
    crate::logging::init_cli_with_config(Some(&config));

    match sub {
        LedgerCmd::Render {
            experiment,
            slug,
            dry_run,
        } => {
            let server = super::experiment::build_cli_server(config).await?;
            let args = serde_json::json!({
                "experiment_id": experiment,
                "slug": slug,
                "dry_run": dry_run,
            });
            match server.call_tool_cli("experiment_render_ledger", args).await {
                Ok(r) => {
                    super::experiment::print_result("ledger", &r);
                    Ok(())
                }
                Err(e) => anyhow::bail!("render_ledger: {}", e.message),
            }
        }
        LedgerCmd::Import { files } => {
            if files.is_empty() {
                anyhow::bail!("provide one or more ledger .md files to inspect");
            }
            for f in &files {
                let content = std::fs::read_to_string(f)
                    .with_context(|| format!("reading {}", f.display()))?;
                let fm = frontmatter::parse(&content);
                match fm.experiment_slug() {
                    Some(slug) => {
                        println!(
                            "{}: linked experiment '{}' (title: {}, kind: {}, verdict: {})",
                            f.display(),
                            slug,
                            fm.get("title").unwrap_or("?"),
                            fm.get("kind").unwrap_or("?"),
                            fm.get("verdict").unwrap_or("?"),
                        );
                    }
                    None => {
                        println!(
                            "{}: prose ledger (no pgmcp_experiment frontmatter) — indexed as text; no structured import",
                            f.display()
                        );
                    }
                }
            }
            Ok(())
        }
    }
}
