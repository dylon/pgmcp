//! REST API handlers for the pgmcp daemon.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use super::ApiState;
use crate::db::queries::{StatusSnapshot, status_snapshot};

// ============================================================================
// POST /api/search — Semantic search
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub limit: Option<i32>,
    pub project: Option<String>,
    pub language: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResultItem>,
}

#[derive(Debug, Serialize)]
pub struct SearchResultItem {
    pub file_path: String,
    pub chunk: String,
    pub similarity: f64,
    pub language: String,
}

pub async fn search(
    State(state): State<ApiState>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, (StatusCode, String)> {
    let limit = req.limit.unwrap_or(5);

    // Embed the query
    let embedding = state
        .query_embedder
        .embed_query(req.query.clone())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Embedding failed: {}", e),
            )
        })?;

    let ef_search = state.config.load().vector.ef_search;
    let results = state
        .db
        .semantic_search(
            &embedding,
            limit,
            req.language.as_deref(),
            req.project.as_deref(),
            ef_search,
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Search failed: {}", e),
            )
        })?;

    let items: Vec<SearchResultItem> = results
        .into_iter()
        .map(|r| SearchResultItem {
            file_path: r.path,
            chunk: r.chunk_content,
            similarity: r.score.unwrap_or(0.0),
            language: r.language,
        })
        .collect();

    Ok(Json(SearchResponse { results: items }))
}

// ============================================================================
// GET /api/context?cwd=/path — Project context
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct ContextQuery {
    pub cwd: String,
    pub depth: Option<i32>,
}

#[derive(Debug, Serialize)]
pub struct ContextResponse {
    pub found: bool,
    pub project: Option<ProjectContext>,
    pub indexed_projects: Option<Vec<ProjectSummary>>,
}

#[derive(Debug, Serialize)]
pub struct ProjectContext {
    pub name: String,
    pub path: String,
    pub file_count: i64,
    pub last_scanned: Option<String>,
    pub languages: Vec<LanguageEntry>,
    pub tree: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct LanguageEntry {
    pub language: String,
    pub count: i64,
}

#[derive(Debug, Serialize)]
pub struct ProjectSummary {
    pub name: String,
    pub path: String,
    pub file_count: i64,
}

pub async fn context(
    State(state): State<ApiState>,
    Query(params): Query<ContextQuery>,
) -> Result<Json<ContextResponse>, (StatusCode, String)> {
    let depth = params.depth.unwrap_or(3);

    let cwd_normalized = if params.cwd.ends_with('/') {
        params.cwd.clone()
    } else {
        format!("{}/", params.cwd)
    };

    let project = state
        .db
        .find_project_by_cwd(&cwd_normalized)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Query failed: {}", e),
            )
        })?;

    match project {
        Some(p) => {
            let languages = state.db.language_summary(&p.name).await.map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Language query failed: {}", e),
                )
            })?;

            let tree = state.db.project_tree(&p.name, depth).await.map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Tree query failed: {}", e),
                )
            })?;

            Ok(Json(ContextResponse {
                found: true,
                project: Some(ProjectContext {
                    name: p.name,
                    path: p.path,
                    file_count: p.file_count.unwrap_or(0),
                    last_scanned: p
                        .last_scanned_at
                        .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string()),
                    languages: languages
                        .into_iter()
                        .map(|l| LanguageEntry {
                            language: l.language,
                            count: l.count,
                        })
                        .collect(),
                    tree,
                }),
                indexed_projects: None,
            }))
        }
        None => {
            let projects = state.db.list_projects().await.map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("List projects failed: {}", e),
                )
            })?;

            Ok(Json(ContextResponse {
                found: false,
                project: None,
                indexed_projects: Some(
                    projects
                        .into_iter()
                        .map(|p| ProjectSummary {
                            name: p.name,
                            path: p.path,
                            file_count: p.file_count.unwrap_or(0),
                        })
                        .collect(),
                ),
            }))
        }
    }
}

// ============================================================================
// GET /api/status — Daemon health & model-state snapshot
// ============================================================================

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    /// Daemon-side runtime fields. None of these are persisted; they
    /// only make sense while the daemon is running.
    pub daemon: DaemonInfo,
    /// Connection details with the password redacted.
    pub database: DatabaseInfo,
    /// Embedding model info (model name, dim, pool size, backend, device).
    pub embeddings: EmbeddingsInfo,
    /// Per-pool capacity for the three-pool architecture
    /// (InferencePool / CronPool / GeneralPool).
    pub pools: PoolsInfo,
    /// Cron-job tunables that affect cross-project similarity output.
    pub similarity_config: SimilarityConfigInfo,
    /// Per-table counts + freshness timestamps from `status_snapshot`.
    pub model_state: StatusSnapshot,
    /// Live in-process counters from `StatsTracker`.
    pub counters: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct DaemonInfo {
    pub version: &'static str,
    pub uptime_secs: u64,
    pub current_rss_bytes: u64,
    pub peak_rss_bytes: u64,
    pub heavy_cron_running: bool,
    pub http_mcp_sessions: u64,
    /// MCP HTTP listener address (`mcp.host:mcp.port`).
    pub bind_addr: String,
    /// Path to the daemon log file (config.logging.file).
    pub log_path: String,
}

#[derive(Debug, Serialize)]
pub struct DatabaseInfo {
    pub url: String,
    pub host: String,
    pub port: u16,
    pub name: String,
    pub max_connections: u32,
    pub pool_size: u32,
    pub pool_idle: usize,
    /// `pool_size - pool_idle` — connections currently checked out.
    pub pool_active: u32,
    pub server_version: Option<String>,
    pub vector_extension_version: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EmbeddingsInfo {
    pub model: String,
    pub dimensions: usize,
    /// `embeddings.pool_size` — number of `InferencePool` workers, each
    /// owning one BertModel + tokenizer + Device. Surface as
    /// "InferencePool workers" in the CLI rendering.
    pub pool_size: usize,
    /// Inference backend (always "candle" since the Step-1 migration of
    /// the candle plan; surfaced explicitly so operators don't have to
    /// `cargo tree` to find out).
    pub backend: &'static str,
    /// "cuda:0" if `use_gpu = true`, else "cpu". Reflects the
    /// configuration; if CUDA init fails at startup, the worker logs the
    /// error and exits — the daemon does not silently fall back.
    pub device: String,
    /// Tokenizer truncation cap. Inputs that tokenize to more tokens are
    /// truncated.
    pub max_length: usize,
    /// Cap on input texts per `BertModel::forward` call. The full batch
    /// is sliced into chunks of this size to keep attention memory
    /// bounded.
    pub inference_batch_size: usize,
}

/// Per-pool capacity snapshot for the three role-specialized pools.
///
/// `InferencePool` is the GPU-bound pool — workers own ONNX/candle
/// sessions and run the full file-indexing pipeline end-to-end.
/// `CronPool` is a small dedicated pool that serves cron-task bodies so
/// a heavy `block_on` job doesn't stall light cleanup tasks. `GeneralPool`
/// is the catch-all CPU-bound pool used for parallel betweenness
/// centrality and similar non-GPU non-cron work.
#[derive(Debug, Serialize)]
pub struct PoolsInfo {
    pub inference: InferencePoolInfo,
    pub cron: CronPoolInfo,
    pub general: GeneralPoolInfo,
}

#[derive(Debug, Serialize)]
pub struct InferencePoolInfo {
    /// Configured worker count (`embeddings.pool_size`).
    pub configured_workers: usize,
}

#[derive(Debug, Serialize)]
pub struct CronPoolInfo {
    /// Hardcoded; see `src/cli/daemon.rs` (currently 2).
    pub configured_workers: usize,
}

#[derive(Debug, Serialize)]
pub struct GeneralPoolInfo {
    pub min_threads: usize,
    pub max_threads: usize,
    /// Live count from `stats.active_work_pool_threads` — the GeneralPool
    /// scaling monitor parks/unparks workers as RSS pressure rises and
    /// falls.
    pub active_workers: u64,
    /// Live count from `stats.work_pool_queue_depth`.
    pub queue_depth: u64,
}

#[derive(Debug, Serialize)]
pub struct SimilarityConfigInfo {
    pub threshold: f64,
    pub top_k: i32,
}

pub async fn status(
    State(state): State<ApiState>,
) -> Result<Json<StatusResponse>, (StatusCode, String)> {
    let pool = state.db.pool().ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "status endpoint requires a real PgPool DbClient (mock unsupported)".to_string(),
        )
    })?;

    let snapshot = status_snapshot(pool).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("status_snapshot failed: {}", e),
        )
    })?;

    let cfg = state.config.load();
    let db_cfg = &cfg.database;

    let daemon = DaemonInfo {
        version: env!("CARGO_PKG_VERSION"),
        uptime_secs: state.stats.uptime_start.elapsed().as_secs(),
        current_rss_bytes: state
            .stats
            .current_rss_bytes
            .load(std::sync::atomic::Ordering::Acquire),
        peak_rss_bytes: state
            .stats
            .peak_rss_bytes
            .load(std::sync::atomic::Ordering::Acquire),
        heavy_cron_running: state
            .stats
            .heavy_cron_running
            .load(std::sync::atomic::Ordering::Acquire),
        http_mcp_sessions: state
            .stats
            .http_mcp_sessions
            .load(std::sync::atomic::Ordering::Acquire),
        bind_addr: format!("{}:{}", cfg.mcp.host, cfg.mcp.port),
        log_path: cfg.logging.file.clone(),
    };

    let pool_size = pool.size();
    let pool_idle = pool.num_idle();
    let pool_active = pool_size.saturating_sub(pool_idle as u32);

    let database = DatabaseInfo {
        url: db_cfg.connection_url_redacted(),
        host: db_cfg.host.clone(),
        port: db_cfg.port,
        name: db_cfg.name.clone(),
        max_connections: db_cfg.max_connections,
        pool_size,
        pool_idle,
        pool_active,
        server_version: snapshot.server_version.clone(),
        vector_extension_version: snapshot.vector_extension_version.clone(),
    };

    let device = if cfg.embeddings.use_gpu {
        "cuda:0".to_string()
    } else {
        "cpu".to_string()
    };
    let embeddings = EmbeddingsInfo {
        model: cfg.embeddings.model.clone(),
        dimensions: cfg.embeddings.dimensions,
        pool_size: cfg.embeddings.pool_size,
        backend: "candle",
        device,
        max_length: cfg.embeddings.max_length,
        inference_batch_size: cfg.embeddings.inference_batch_size,
    };

    let pools = PoolsInfo {
        inference: InferencePoolInfo {
            configured_workers: cfg.embeddings.pool_size,
        },
        cron: CronPoolInfo {
            // Mirrors the hardcoded `cron_pool` in `src/cli/daemon.rs`.
            configured_workers: 2,
        },
        general: GeneralPoolInfo {
            min_threads: cfg.work_pool.min_threads,
            max_threads: cfg.work_pool.resolved_max_threads(),
            active_workers: state
                .stats
                .active_work_pool_threads
                .load(std::sync::atomic::Ordering::Acquire),
            queue_depth: state
                .stats
                .work_pool_queue_depth
                .load(std::sync::atomic::Ordering::Acquire),
        },
    };

    let similarity_config = SimilarityConfigInfo {
        threshold: cfg.cron.similarity_threshold,
        top_k: cfg.cron.similarity_top_k,
    };

    Ok(Json(StatusResponse {
        daemon,
        database,
        embeddings,
        pools,
        similarity_config,
        model_state: snapshot,
        counters: state.stats.snapshot(),
    }))
}
