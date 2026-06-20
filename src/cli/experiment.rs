//! `pgmcp experiment` — the agent-driven experiment executor + importer.
//!
//! `run` reads a JSON `RunRequest` (arms + plan), executes the arms locally
//! with CPU pinning + governor enforcement via
//! [`crate::experiment::runner`], then submits the raw samples through the
//! protocol-enforcing `experiment_record_measurement` tool (and optionally
//! `experiment_decide`). `ingest` imports an existing hyperfine/criterion
//! artifact as samples. The thin CRUD operations (open/protocol/search/get/…)
//! are available via `pgmcp tool experiment_<op> key=value`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use arc_swap::ArcSwap;
use clap::Subcommand;

use crate::config::Config;
use crate::context::SystemContext;
use crate::experiment::extract;
use crate::experiment::runner;
use crate::experiment::spec::RunRequest;
use crate::{db, embed, mcp, stats};

#[derive(Subcommand, Debug)]
pub enum ExperimentCmd {
    /// Execute control/treatment arms from a JSON spec, then submit the samples.
    Run {
        /// Path to a RunRequest JSON: {experiment_id, hypothesis_id, arms, plan, decide}.
        #[arg(long)]
        spec: PathBuf,
    },
    /// Import a hyperfine/criterion artifact file as samples for one arm.
    Ingest {
        /// Artifact file (hyperfine --export-json or criterion sample.json).
        path: PathBuf,
        /// Artifact kind: hyperfine | criterion.
        #[arg(long)]
        kind: String,
        /// Target experiment id.
        #[arg(long)]
        experiment: i64,
        /// Hypothesis id the samples attach to.
        #[arg(long)]
        hypothesis: Option<i64>,
        /// Arm label (default "treatment").
        #[arg(long, default_value = "treatment")]
        arm: String,
        /// Metric name to record the samples under.
        #[arg(long)]
        metric: String,
    },
}

/// Build the same CLI-mode `McpServer` that `pgmcp tool` uses (lazy embedder).
pub(crate) async fn build_cli_server(config: Config) -> anyhow::Result<mcp::server::McpServer> {
    let pool = db::pool::create_pool(&config.database).await?;
    db::migrations::run_migrations(&pool, &config.vector, false).await?;
    let stats = Arc::new(stats::tracker::StatsTracker::new());
    let config_arc = Arc::new(ArcSwap::from_pointee(config));
    let log_broadcaster = Arc::new(mcp::logging::LogBroadcaster::new());
    let task_store = Arc::new(mcp::tasks::TaskStore::new());
    let db: Arc<dyn db::DbClient> = Arc::new(pool);
    let lifecycle = crate::daemon_state::DaemonLifecycle::new();
    lifecycle.transition(crate::daemon_state::DaemonPhase::Ready);
    let ctx = SystemContext::production(
        db,
        embed::EmbedSource::lazy(config_arc.load().embeddings.clone()),
        stats,
        config_arc,
        log_broadcaster,
        task_store,
        lifecycle,
    );
    Ok(mcp::server::McpServer::new(ctx))
}

pub(crate) fn print_result(label: &str, result: &rmcp::model::CallToolResult) {
    println!("=== {label} ===");
    for content in &result.content {
        if let rmcp::model::RawContent::Text(t) = &content.raw {
            match serde_json::from_str::<serde_json::Value>(&t.text) {
                Ok(j) => println!(
                    "{}",
                    serde_json::to_string_pretty(&j).unwrap_or_else(|_| t.text.clone())
                ),
                Err(_) => println!("{}", t.text),
            }
        }
    }
}

pub async fn run(config_override: Option<&Path>, sub: ExperimentCmd) -> anyhow::Result<()> {
    let config = Config::load(config_override)?;
    crate::logging::init_cli_with_config(Some(&config));
    let server = build_cli_server(config).await?;

    match sub {
        ExperimentCmd::Run { spec } => cmd_run(&server, &spec).await,
        ExperimentCmd::Ingest {
            path,
            kind,
            experiment,
            hypothesis,
            arm,
            metric,
        } => cmd_ingest(&server, &path, &kind, experiment, hypothesis, &arm, &metric).await,
    }
}

async fn cmd_run(server: &mcp::server::McpServer, spec_path: &Path) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(spec_path)
        .with_context(|| format!("reading run spec {}", spec_path.display()))?;
    let req: RunRequest = serde_json::from_str(&text).context("parsing RunRequest JSON")?;

    let experiment_id = req.experiment_id.context(
        "run spec must include experiment_id (resolve a slug via `pgmcp tool experiment_get`)",
    )?;

    println!(
        "Executing {} arm(s) × {} replicate(s) (warmup {})…",
        req.arms.len(),
        req.plan.replicates,
        req.plan.warmup
    );
    let outcome =
        runner::execute(&req.arms, &req.plan).map_err(|e| anyhow::anyhow!("runner: {e}"))?;
    for w in &outcome.warnings {
        eprintln!("warning: {w}");
    }

    // Submit each arm's samples through the protocol-enforcing record tool.
    for arm in &req.arms {
        let Some(samples) = outcome.samples.get(&arm.name) else {
            continue;
        };
        let args = serde_json::json!({
            "experiment_id": experiment_id,
            "hypothesis_id": req.hypothesis_id,
            "arm_label": arm.name,
            "arm_kind": arm.kind,
            "metric": outcome.metric_name,
            "samples": samples,
            "source": "external_benchmark",
            "host_meta": outcome.host_meta,
            "git_ref": arm.git_ref,
        });
        match server
            .call_tool_cli("experiment_record_measurement", args)
            .await
        {
            Ok(r) => print_result(
                &format!("recorded {} ({} samples)", arm.name, samples.len()),
                &r,
            ),
            Err(e) => anyhow::bail!("record_measurement for arm '{}': {}", arm.name, e.message),
        }
    }

    if req.decide {
        let hyp = req
            .hypothesis_id
            .context("decide=true requires hypothesis_id in the spec")?;
        let args = serde_json::json!({ "hypothesis_id": hyp });
        match server.call_tool_cli("experiment_decide", args).await {
            Ok(r) => print_result("decision", &r),
            Err(e) => anyhow::bail!("decide: {}", e.message),
        }
    }
    Ok(())
}

async fn cmd_ingest(
    server: &mcp::server::McpServer,
    path: &Path,
    kind: &str,
    experiment: i64,
    hypothesis: Option<i64>,
    arm: &str,
    metric: &str,
) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading artifact {}", path.display()))?;
    let samples = match kind {
        "hyperfine" => extract::parse_hyperfine_times(&content),
        "criterion" => extract::parse_criterion_samples(&content),
        other => Err(format!(
            "unsupported ingest kind '{other}' (hyperfine | criterion)"
        )),
    }
    .map_err(|e| anyhow::anyhow!(e))?;

    let arm_kind = if arm == "control" {
        "control"
    } else {
        "treatment"
    };
    let rec_args = serde_json::json!({
        "experiment_id": experiment,
        "hypothesis_id": hypothesis,
        "arm_label": arm,
        "arm_kind": arm_kind,
        "metric": metric,
        "samples": samples,
        "source": kind,
    });
    match server
        .call_tool_cli("experiment_record_measurement", rec_args)
        .await
    {
        Ok(r) => print_result(&format!("recorded {} samples", samples.len()), &r),
        Err(e) => anyhow::bail!("record_measurement: {}", e.message),
    }

    // Also archive the raw artifact (parsed into a metrics summary).
    let art_args = serde_json::json!({
        "experiment_id": experiment,
        "kind": kind,
        "tool": kind,
        "label": format!("{arm}:{metric}"),
        "content": content,
        "parse": true,
    });
    match server
        .call_tool_cli("experiment_log_artifact", art_args)
        .await
    {
        Ok(r) => print_result("artifact", &r),
        Err(e) => eprintln!("warning: log_artifact failed: {}", e.message),
    }
    Ok(())
}
