//! `status` subcommand: print model & runtime state from the daemon
//! (via `GET /api/status`), with a DB-only fallback when the daemon
//! is unreachable.
//!
//! Usage:
//!   pgmcp status                  # all sections
//!   pgmcp status topics           # just one model section
//!   pgmcp status --json           # full snapshot as pretty JSON
//!
//! Sections (in fixed order): daemon, database, embeddings, topics,
//! similarity, graph, git.

use std::path::Path;

use crate::config::Config;
use chrono::{DateTime, Utc};
use serde::Deserialize;

/// Top-level request entry. `model` filters the rendered sections;
/// `json` prints the full snapshot as pretty JSON regardless of model.
pub async fn run(
    config_override: Option<&Path>,
    model: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    crate::logging::init_cli();
    let config = Config::load(config_override)?;
    // /api/* routes are mounted on the MCP Streamable HTTP server
    // (config.mcp.host:port), NOT on the Prometheus metrics server.
    let url = format!("http://{}:{}/api/status", config.mcp.host, config.mcp.port);

    // Prefer the daemon — only it has live counters and session count.
    // On any error, fall back to building the snapshot directly from
    // the database.
    let snapshot = match http_get(&url).await {
        Ok(body) => match serde_json::from_str::<StatusResponse>(&body) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("warning: daemon at {url} returned unparseable JSON: {e}");
                None
            }
        },
        Err(e) => {
            eprintln!("warning: daemon at {url} unreachable: {e}");
            eprintln!("         falling back to DB-only snapshot (no live counters / sessions).");
            None
        }
    };

    let snapshot = match snapshot {
        Some(s) => s,
        None => fallback_snapshot(&config).await?,
    };

    if json {
        let pretty = serde_json::to_string_pretty(&snapshot)?;
        println!("{}", pretty);
        return Ok(());
    }

    print_text(&snapshot, model.as_deref());
    Ok(())
}

// ============================================================================
// Wire types — must match `src/api/handlers.rs::StatusResponse`
// ============================================================================

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct StatusResponse {
    pub daemon: DaemonInfo,
    pub database: DatabaseInfo,
    pub embeddings: EmbeddingsInfo,
    pub pools: PoolsInfo,
    pub similarity_config: SimilarityConfigInfo,
    pub model_state: ModelStateSnapshot,
    /// Live in-process counters from `StatsTracker::snapshot()`. Null
    /// in DB-fallback mode.
    pub counters: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct DaemonInfo {
    pub version: String,
    pub uptime_secs: u64,
    pub current_rss_bytes: u64,
    pub peak_rss_bytes: u64,
    pub heavy_cron_running: bool,
    pub http_mcp_sessions: u64,
    pub bind_addr: String,
    pub log_path: String,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct DatabaseInfo {
    pub url: String,
    pub host: String,
    pub port: u16,
    pub name: String,
    pub max_connections: u32,
    pub pool_size: u32,
    pub pool_idle: usize,
    pub pool_active: u32,
    pub server_version: Option<String>,
    pub vector_extension_version: Option<String>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct EmbeddingsInfo {
    pub model: String,
    pub dimensions: usize,
    pub pool_size: usize,
    #[serde(default = "default_backend")]
    pub backend: String,
    #[serde(default = "default_device")]
    pub device: String,
    #[serde(default = "default_max_length")]
    pub max_length: usize,
    #[serde(default = "default_inference_batch_size")]
    pub inference_batch_size: usize,
}

fn default_backend() -> String {
    "candle".into()
}
fn default_device() -> String {
    "cpu".into()
}
fn default_max_length() -> usize {
    512
}
fn default_inference_batch_size() -> usize {
    8
}

/// Mirrors `src/api/handlers.rs::PoolsInfo`. Decoded from `/api/status`
/// or constructed in DB-only fallback with config-only knowledge (no
/// live worker counts).
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct PoolsInfo {
    pub inference: InferencePoolInfo,
    pub cron: CronPoolInfo,
    pub general: GeneralPoolInfo,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct InferencePoolInfo {
    pub configured_workers: usize,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct CronPoolInfo {
    pub configured_workers: usize,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct GeneralPoolInfo {
    pub min_threads: usize,
    pub max_threads: usize,
    pub active_workers: u64,
    pub queue_depth: u64,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct SimilarityConfigInfo {
    pub threshold: f64,
    pub top_k: i32,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ModelStateSnapshot {
    pub project_count: i64,
    pub indexed_file_count: i64,
    pub chunk_count: i64,
    pub git_commit_count: i64,
    pub git_commit_chunk_count: i64,
    pub topic_count_global: i64,
    pub topic_count_total: i64,
    pub topic_assignments_total: i64,
    pub topic_last_computed: Option<DateTime<Utc>>,
    pub topic_noise_chunk_count: i64,
    pub topic_breakdown_by_scope: Vec<TopicScopeStat>,
    pub similarity_pair_count: i64,
    pub similarity_distinct_files: i64,
    pub similarity_last_computed: Option<DateTime<Utc>>,
    pub file_metric_count: i64,
    pub graph_edge_count: i64,
    pub graph_edges_by_type: Vec<EdgeTypeCount>,
    pub graph_metric_last_computed: Option<DateTime<Utc>>,
    pub graph_edge_last_computed: Option<DateTime<Utc>>,
    pub blame_coverage_with: i64,
    pub blame_coverage_total: i64,
    pub per_project: Vec<PerProjectStat>,
    pub git_per_project: Vec<GitProjectStat>,
    pub last_indexed_at: Option<DateTime<Utc>>,
    pub server_version: Option<String>,
    pub vector_extension_version: Option<String>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct EdgeTypeCount {
    pub edge_type: String,
    pub count: i64,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct TopicScopeStat {
    pub scope: String,
    pub topic_count: i64,
    pub last_computed: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct PerProjectStat {
    pub project_name: String,
    pub indexed_file_count: i64,
    pub chunk_count: i64,
    pub file_metric_count: i64,
    pub last_indexed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct GitProjectStat {
    pub project_name: String,
    pub commit_count: i64,
    pub last_commit_hash: Option<String>,
    pub last_commit_date: Option<DateTime<Utc>>,
}

// ============================================================================
// DB-only fallback snapshot
// ============================================================================

async fn fallback_snapshot(config: &Config) -> anyhow::Result<StatusResponse> {
    // Route through `create_pool` so the fallback inherits the same
    // session timeouts and acquire safeguards as the daemon pool — a
    // status check should fail fast on a degraded DB, not hang.
    let mut db_cfg = config.database.clone();
    db_cfg.max_connections = 2;
    let pool = crate::db::pool::create_pool(&db_cfg).await?;

    let snap = crate::db::queries::status_snapshot(&pool).await?;
    let pool_size = pool.size();
    let pool_idle = pool.num_idle();
    let pool_active = pool_size.saturating_sub(pool_idle as u32);

    Ok(StatusResponse {
        daemon: DaemonInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            uptime_secs: 0,
            current_rss_bytes: 0,
            peak_rss_bytes: 0,
            heavy_cron_running: false,
            http_mcp_sessions: 0,
            bind_addr: format!("{}:{}", config.mcp.host, config.mcp.port),
            log_path: config.logging.file.clone(),
        },
        database: DatabaseInfo {
            url: config.database.connection_url_redacted(),
            host: config.database.host.clone(),
            port: config.database.port,
            name: config.database.name.clone(),
            max_connections: config.database.max_connections,
            pool_size,
            pool_idle,
            pool_active,
            server_version: snap.server_version.clone(),
            vector_extension_version: snap.vector_extension_version.clone(),
        },
        embeddings: EmbeddingsInfo {
            model: config.embeddings.model.clone(),
            dimensions: config.embeddings.dimensions,
            pool_size: config.embeddings.pool_size,
            backend: "candle".into(),
            device: if config.embeddings.use_gpu {
                "cuda:0".into()
            } else {
                "cpu".into()
            },
            max_length: config.embeddings.max_length,
            inference_batch_size: config.embeddings.inference_batch_size,
        },
        pools: PoolsInfo {
            inference: InferencePoolInfo {
                configured_workers: config.embeddings.pool_size,
            },
            // Mirrors the hardcoded `cron_pool` in `src/cli/daemon.rs`.
            cron: CronPoolInfo {
                configured_workers: 2,
            },
            general: GeneralPoolInfo {
                min_threads: config.work_pool.min_threads,
                max_threads: config.work_pool.resolved_max_threads(),
                // No live counts in DB-fallback mode.
                active_workers: 0,
                queue_depth: 0,
            },
        },
        similarity_config: SimilarityConfigInfo {
            threshold: config.cron.similarity_threshold,
            top_k: config.cron.similarity_top_k,
        },
        model_state: ModelStateSnapshot {
            project_count: snap.project_count,
            indexed_file_count: snap.indexed_file_count,
            chunk_count: snap.chunk_count,
            git_commit_count: snap.git_commit_count,
            git_commit_chunk_count: snap.git_commit_chunk_count,
            topic_count_global: snap.topic_count_global,
            topic_count_total: snap.topic_count_total,
            topic_assignments_total: snap.topic_assignments_total,
            topic_last_computed: snap.topic_last_computed,
            topic_noise_chunk_count: snap.topic_noise_chunk_count,
            topic_breakdown_by_scope: snap
                .topic_breakdown_by_scope
                .into_iter()
                .map(|s| TopicScopeStat {
                    scope: s.scope,
                    topic_count: s.topic_count,
                    last_computed: s.last_computed,
                })
                .collect(),
            similarity_pair_count: snap.similarity_pair_count,
            similarity_distinct_files: snap.similarity_distinct_files,
            similarity_last_computed: snap.similarity_last_computed,
            file_metric_count: snap.file_metric_count,
            graph_edge_count: snap.graph_edge_count,
            graph_edges_by_type: snap
                .graph_edges_by_type
                .into_iter()
                .map(|e| EdgeTypeCount {
                    edge_type: e.edge_type,
                    count: e.count,
                })
                .collect(),
            graph_metric_last_computed: snap.graph_metric_last_computed,
            graph_edge_last_computed: snap.graph_edge_last_computed,
            blame_coverage_with: snap.blame_coverage_with,
            blame_coverage_total: snap.blame_coverage_total,
            per_project: snap
                .per_project
                .into_iter()
                .map(|p| PerProjectStat {
                    project_name: p.project_name,
                    indexed_file_count: p.indexed_file_count,
                    chunk_count: p.chunk_count,
                    file_metric_count: p.file_metric_count,
                    last_indexed_at: p.last_indexed_at,
                })
                .collect(),
            git_per_project: snap
                .git_per_project
                .into_iter()
                .map(|g| GitProjectStat {
                    project_name: g.project_name,
                    commit_count: g.commit_count,
                    last_commit_hash: g.last_commit_hash,
                    last_commit_date: g.last_commit_date,
                })
                .collect(),
            last_indexed_at: snap.last_indexed_at,
            server_version: snap.server_version,
            vector_extension_version: snap.vector_extension_version,
        },
        counters: serde_json::Value::Null,
    })
}

// ============================================================================
// Text rendering
// ============================================================================

const SECTION_NAMES: &[&str] = &[
    "daemon",
    "database",
    "embeddings",
    "pools",
    "topics",
    "similarity",
    "graph",
    "git",
];

fn print_text(snap: &StatusResponse, model: Option<&str>) {
    let normalized = model.map(|m| m.to_ascii_lowercase());
    let want = |section: &str| -> bool {
        match normalized.as_deref() {
            None => true,
            Some(m) => m == section,
        }
    };

    if let Some(m) = normalized.as_deref()
        && !SECTION_NAMES.contains(&m)
    {
        eprintln!(
            "unknown model `{m}` — known sections: {}",
            SECTION_NAMES.join(", ")
        );
        return;
    }

    println!("pgmcp Status");
    println!("{}", "=".repeat(50));

    if want("daemon") {
        println!("\n  Daemon:");
        kv("version", &snap.daemon.version);
        kv("uptime", &fmt_secs(snap.daemon.uptime_secs));
        kv("bind addr (MCP HTTP)", &snap.daemon.bind_addr);
        kv("log path", &snap.daemon.log_path);
        kv("current rss", &fmt_bytes(snap.daemon.current_rss_bytes));
        kv("peak rss", &fmt_bytes(snap.daemon.peak_rss_bytes));
        kv(
            "heavy cron running",
            if snap.daemon.heavy_cron_running {
                "yes"
            } else {
                "no"
            },
        );
        kv(
            "connected MCP clients (HTTP)",
            &snap.daemon.http_mcp_sessions.to_string(),
        );
        // RUNPATHs (CUDA/ort) and BLAS link (AOCL-BLIS) are wired by build.rs
        // and inspectable via `readelf -d $(which pgmcp) | grep -E 'NEEDED|RUNPATH'`
        // — too noisy to surface inline.
    }

    if want("database") {
        println!("\n  Database:");
        kv("url", &snap.database.url);
        kv("host", &snap.database.host);
        kv("port", &snap.database.port.to_string());
        kv("name", &snap.database.name);
        kv(
            "max connections",
            &snap.database.max_connections.to_string(),
        );
        kv("pool size", &snap.database.pool_size.to_string());
        kv("pool active", &snap.database.pool_active.to_string());
        kv("pool idle", &snap.database.pool_idle.to_string());
        kv(
            "server version",
            snap.database.server_version.as_deref().unwrap_or("?"),
        );
        kv(
            "vector ext version",
            snap.database
                .vector_extension_version
                .as_deref()
                .unwrap_or("?"),
        );
    }

    if want("embeddings") {
        println!("\n  Embeddings:");
        kv("model", &snap.embeddings.model);
        kv("backend", &snap.embeddings.backend);
        kv("device", &snap.embeddings.device);
        kv("dimensions", &snap.embeddings.dimensions.to_string());
        kv(
            "max_length (tokens)",
            &snap.embeddings.max_length.to_string(),
        );
        kv(
            "inference_batch_size",
            &snap.embeddings.inference_batch_size.to_string(),
        );
        kv(
            "InferencePool workers",
            &snap.embeddings.pool_size.to_string(),
        );
        kv("projects", &snap.model_state.project_count.to_string());
        kv(
            "files indexed",
            &snap.model_state.indexed_file_count.to_string(),
        );
        kv("chunks", &snap.model_state.chunk_count.to_string());
        kv(
            "last index",
            &snap
                .model_state
                .last_indexed_at
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "never".into()),
        );
        if !snap.model_state.per_project.is_empty() {
            println!("    per-project:");
            for p in &snap.model_state.per_project {
                println!(
                    "      {:<32} files={}, chunks={}, metrics={}",
                    p.project_name, p.indexed_file_count, p.chunk_count, p.file_metric_count,
                );
            }
        }
    }

    if want("pools") {
        println!("\n  Pools (three-pool architecture):");
        println!("    InferencePool (GPU-bound — file indexing + query embed + GPU FCM):");
        kv(
            "  configured workers",
            &snap.pools.inference.configured_workers.to_string(),
        );
        println!("    CronPool (cron task bodies — keeps light jobs unstalled):");
        kv(
            "  configured workers",
            &snap.pools.cron.configured_workers.to_string(),
        );
        println!("    GeneralPool (CPU misc — parallel betweenness, ad-hoc work):");
        kv("  min threads", &snap.pools.general.min_threads.to_string());
        kv("  max threads", &snap.pools.general.max_threads.to_string());
        kv(
            "  active workers (live)",
            &snap.pools.general.active_workers.to_string(),
        );
        kv(
            "  queue depth (live)",
            &snap.pools.general.queue_depth.to_string(),
        );
    }

    if want("topics") {
        println!("\n  Topics:");
        kv(
            "global topics (scope='global')",
            &snap.model_state.topic_count_global.to_string(),
        );
        kv(
            "total topics (all scopes)",
            &snap.model_state.topic_count_total.to_string(),
        );
        kv(
            "fuzzy assignments",
            &snap.model_state.topic_assignments_total.to_string(),
        );
        kv(
            "noise chunks (unassigned)",
            &snap.model_state.topic_noise_chunk_count.to_string(),
        );
        kv(
            "last computed (any scope)",
            &snap
                .model_state
                .topic_last_computed
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "never".into()),
        );
        if !snap.model_state.topic_breakdown_by_scope.is_empty() {
            println!("    per-scope:");
            for s in &snap.model_state.topic_breakdown_by_scope {
                println!(
                    "      {:<32} K={}, last={}",
                    s.scope,
                    s.topic_count,
                    s.last_computed
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_else(|| "never".into()),
                );
            }
        }
    }

    if want("similarity") {
        println!("\n  Cross-project similarity:");
        kv(
            "materialized pairs",
            &snap.model_state.similarity_pair_count.to_string(),
        );
        kv(
            "distinct files participating",
            &snap.model_state.similarity_distinct_files.to_string(),
        );
        kv(
            "last computed",
            &snap
                .model_state
                .similarity_last_computed
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "never".into()),
        );
        kv(
            "configured threshold",
            &format!("{:.2}", snap.similarity_config.threshold),
        );
        kv(
            "configured top_k",
            &snap.similarity_config.top_k.to_string(),
        );
    }

    if want("graph") {
        println!("\n  Graph:");
        kv(
            "files with metrics",
            &snap.model_state.file_metric_count.to_string(),
        );
        kv(
            "total edges",
            &snap.model_state.graph_edge_count.to_string(),
        );
        for et in &snap.model_state.graph_edges_by_type {
            kv(&format!("edges ({})", et.edge_type), &et.count.to_string());
        }
        kv(
            "metrics last computed",
            &snap
                .model_state
                .graph_metric_last_computed
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "never".into()),
        );
        kv(
            "edges last computed",
            &snap
                .model_state
                .graph_edge_last_computed
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "never".into()),
        );
    }

    if want("git") {
        println!("\n  Git history:");
        kv(
            "commits indexed",
            &snap.model_state.git_commit_count.to_string(),
        );
        kv(
            "commit chunks",
            &snap.model_state.git_commit_chunk_count.to_string(),
        );
        let pct = if snap.model_state.blame_coverage_total > 0 {
            100.0 * snap.model_state.blame_coverage_with as f64
                / snap.model_state.blame_coverage_total as f64
        } else {
            0.0
        };
        kv(
            "blame coverage (chunks)",
            &format!(
                "{}/{} ({:.1}%)",
                snap.model_state.blame_coverage_with, snap.model_state.blame_coverage_total, pct
            ),
        );
        if !snap.model_state.git_per_project.is_empty() {
            println!("    per-project:");
            for g in &snap.model_state.git_per_project {
                let last_hash = g
                    .last_commit_hash
                    .as_deref()
                    .map(|h| &h[..h.len().min(12)])
                    .unwrap_or("?");
                let last_date = g
                    .last_commit_date
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(|| "?".into());
                println!(
                    "      {:<32} commits={}, last={} @ {}",
                    g.project_name, g.commit_count, last_hash, last_date,
                );
            }
        }
    }

    println!();
}

fn kv(key: &str, value: &str) {
    println!("    {:<34} {}", key, value);
}

fn fmt_secs(s: u64) -> String {
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    if h > 0 {
        format!("{h}h{m:02}m{sec:02}s")
    } else if m > 0 {
        format!("{m}m{sec:02}s")
    } else {
        format!("{sec}s")
    }
}

fn fmt_bytes(b: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if b >= GB {
        format!("{:.2} GiB", b as f64 / GB as f64)
    } else if b >= MB {
        format!("{:.2} MiB", b as f64 / MB as f64)
    } else if b >= KB {
        format!("{:.2} KiB", b as f64 / KB as f64)
    } else {
        format!("{b} B")
    }
}

// ============================================================================
// Tiny HTTP GET (no extra deps; mirrors src/stats/cli.rs::reqwest_get)
// ============================================================================

async fn http_get(url: &str) -> anyhow::Result<String> {
    let url = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("Only http:// URLs supported"))?;
    let (host_port, path) = url.split_once('/').unwrap_or((url, "api/status"));

    let stream = tokio::net::TcpStream::connect(host_port).await?;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut reader, mut writer) = stream.into_split();

    let request = format!(
        "GET /{} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
        path, host_port
    );
    writer.write_all(request.as_bytes()).await?;

    let mut response = String::new();
    reader.read_to_string(&mut response).await?;

    let body = response.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    Ok(body)
}
