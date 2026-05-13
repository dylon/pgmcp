//! REST API handlers for the pgmcp daemon.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use super::ApiState;
use crate::daemon_state::DaemonPhase;
use crate::db::queries::{StatusSnapshot, status_snapshot};

// ============================================================================
// GET /health — Cheap liveness probe (no DB queries, no model touch)
// ============================================================================

/// Lightweight liveness probe for k8s probes, systemd watchdogs, uptime
/// monitors, and the `~/.claude/hooks/pgmcp-*.sh` PreToolUse hooks
/// (which check this with a 300 ms timeout before deciding whether to
/// inject pgmcp context). Reads only an atomic phase from the
/// `DaemonLifecycle` — does not touch the DB or any worker pool.
///
/// 200 OK with `{"phase": "ready"}` when the daemon is in the `Ready`
/// phase. 503 SERVICE_UNAVAILABLE with `{"phase": "<label>"}` for any
/// other phase (Initializing/Scanning/Terminating/Defunct).
///
/// Intended to be polled at high frequency. Distinct from `/api/status`,
/// which returns a rich snapshot but issues ~10 SQL `COUNT(*)` queries.
pub async fn health(State(state): State<ApiState>) -> impl IntoResponse {
    let phase = state.lifecycle.current();
    let body = Json(serde_json::json!({ "phase": phase.label() }));
    if phase == DaemonPhase::Ready {
        (StatusCode::OK, body)
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, body)
    }
}

// ============================================================================
// POST /api/grep — Cross-project regex grep (REST mirror of mcp__pgmcp__grep)
// ============================================================================

/// Used by the `~/.claude/hooks/pgmcp-grep-companion.sh` PreToolUse hook
/// when the model issues a broad-path `Grep`. Hook calls this and injects
/// pgmcp's cross-project hits into the model's context alongside the
/// native `Grep` result.
#[derive(Debug, Deserialize)]
pub struct GrepRequest {
    pub pattern: String,
    pub glob: Option<String>,
    pub limit: Option<i32>,
}

#[derive(Debug, Serialize)]
pub struct GrepResponse {
    pub results: Vec<crate::db::queries::GrepResult>,
    pub truncated: bool,
}

pub async fn grep(
    State(state): State<ApiState>,
    Json(req): Json<GrepRequest>,
) -> Result<Json<GrepResponse>, (StatusCode, String)> {
    // Clamp limit to [1, 50] — the hook caps its own injection at 10, but
    // give a small buffer for direct callers.
    let limit = req.limit.unwrap_or(10).clamp(1, 50);

    // The /api/grep endpoint is consumed by ~/.claude/hooks/pgmcp-grep-companion.sh
    // which doesn't currently expose dedupe; default false preserves
    // existing behavior. The hook can opt in later via a query param.
    let results = state
        .db
        .grep_search(&req.pattern, req.glob.as_deref(), limit, false)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("grep_search failed: {}", e),
            )
        })?;

    let truncated = results.len() == limit as usize;
    Ok(Json(GrepResponse { results, truncated }))
}

// ============================================================================
// POST /api/file_envelope — File metadata for the read-context hook
// ============================================================================

/// Compact envelope returned to `~/.claude/hooks/pgmcp-read-context.sh`
/// when the model is about to `Read` a file: language, line count,
/// last_indexed_at. Future expansion will include centrality_rank,
/// top_topics, top_coupled_files, and recent_commits — for now it returns
/// what the trait already exposes via `file_info`.
#[derive(Debug, Deserialize)]
pub struct FileEnvelopeRequest {
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct FileEnvelopeResponse {
    pub found: bool,
    pub info: Option<crate::db::queries::FileInfo>,
}

pub async fn file_envelope(
    State(state): State<ApiState>,
    Json(req): Json<FileEnvelopeRequest>,
) -> Result<Json<FileEnvelopeResponse>, (StatusCode, String)> {
    let info = state.db.file_info(&req.path).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("file_info failed: {}", e),
        )
    })?;

    Ok(Json(FileEnvelopeResponse {
        found: info.is_some(),
        info,
    }))
}

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
    // The /api/search endpoint is consumed by ~/.claude/hooks/pgmcp-rag.sh
    // (UserPromptSubmit). Default dedupe=false preserves existing
    // behavior — the hook can pass a query param later if it wants
    // worktree-collapsed results.
    let results = state
        .db
        .semantic_search(
            &embedding,
            limit,
            req.language.as_deref(),
            req.project.as_deref(),
            ef_search,
            false,
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
// POST /api/session/observe — Session-mandate observation + re-injection
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct ObserveRequest {
    pub session_id: uuid::Uuid,
    pub cwd: String,
    pub prompt: String,
    #[serde(default = "default_true")]
    pub include_rag: bool,
    pub rag_limit: Option<i32>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
pub struct ObserveResponse {
    pub session_id: uuid::Uuid,
    pub prompt_id: i64,
    pub extracted: Vec<crate::sessions::ExtractedMandate>,
    pub active_mandates: Vec<crate::sessions::SessionMandate>,
    pub rag_hits: Vec<SearchResultItem>,
    pub additional_context: String,
}

pub async fn session_observe(
    State(state): State<ApiState>,
    Json(req): Json<ObserveRequest>,
) -> Result<Json<ObserveResponse>, (StatusCode, String)> {
    let pool = state.db.pool().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "raw pool unavailable".to_string(),
    ))?;

    // Resolve project_id from cwd (longest-prefix match).
    let project = state.db.find_project_by_cwd(&req.cwd).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("project lookup failed: {}", e),
        )
    })?;
    let project_id = project.as_ref().map(|p| p.id);

    crate::sessions::upsert_session(pool, req.session_id, &req.cwd, project_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("upsert_session failed: {}", e),
            )
        })?;

    let sha256 = crate::sessions::prompt_sha256(&req.prompt);

    // Embed the prompt for cross-session retrieval (and to populate the
    // vector column on the row we're about to insert).
    let embedding = state
        .query_embedder
        .embed_query(req.prompt.clone())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Embedding failed: {}", e),
            )
        })?;

    let prompt_id = crate::sessions::insert_prompt(
        pool,
        req.session_id,
        &req.prompt,
        &sha256,
        Some(&embedding),
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("insert_prompt failed: {}", e),
        )
    })?;

    let extracted = crate::sessions::extract_mandates(&req.prompt, Some(&req.cwd));
    for m in &extracted {
        let _ = crate::sessions::upsert_mandate(pool, req.session_id, prompt_id, m)
            .await
            .map_err(|e| tracing::warn!(error = %e, "upsert_mandate failed"));
    }

    let active = crate::sessions::list_active_mandates(pool, Some(req.session_id), None, 20)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_active_mandates failed: {}", e),
            )
        })?;

    // Optional RAG hits using the existing semantic_search path.
    let mut rag_hits: Vec<SearchResultItem> = Vec::new();
    if req.include_rag {
        let limit = req.rag_limit.unwrap_or(5).clamp(1, 20);
        let ef_search = state.config.load().vector.ef_search;
        if let Ok(hits) = state
            .db
            .semantic_search(&embedding, limit, None, None, ef_search, false)
            .await
        {
            rag_hits = hits
                .into_iter()
                .map(|r| SearchResultItem {
                    file_path: r.path,
                    chunk: r.chunk_content,
                    similarity: r.score.unwrap_or(0.0),
                    language: r.language,
                })
                .collect();
        }
    }

    // Render the combined `additional_context` Markdown block (≤ 2 KB).
    let mut additional_context = crate::sessions::render_session_mandates_md(&active, 2048);
    if !rag_hits.is_empty() {
        additional_context.push_str("\n## Relevant indexed code (pgmcp RAG)\n\n");
        let budget_remaining = 2048usize.saturating_sub(additional_context.len());
        let mut used = 0;
        for hit in &rag_hits {
            let line = format!("- `{}` (similarity {:.2})\n", hit.file_path, hit.similarity);
            if used + line.len() > budget_remaining {
                break;
            }
            additional_context.push_str(&line);
            used += line.len();
        }
    }

    Ok(Json(ObserveResponse {
        session_id: req.session_id,
        prompt_id,
        extracted,
        active_mandates: active,
        rag_hits,
        additional_context,
    }))
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
// GET /api/mandates?project=name&cwd=/path — Effective mandates
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct MandatesQuery {
    pub project: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MandatesResponse {
    pub requested_project: Option<String>,
    pub requested_cwd: Option<String>,
    pub found_project: bool,
    pub mandates: crate::mandates::MandateBundle,
}

pub async fn mandates(
    State(state): State<ApiState>,
    Query(params): Query<MandatesQuery>,
) -> Result<Json<MandatesResponse>, (StatusCode, String)> {
    let project = crate::mandates::resolve_project_for_mandates(
        state.db.as_ref(),
        params.project.as_deref(),
        params.cwd.as_deref(),
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Project lookup failed: {}", e),
        )
    })?;

    let config = state.config.load();
    let bundle = crate::mandates::resolve_effective_mandates(&config, project.as_ref());

    Ok(Json(MandatesResponse {
        requested_project: params.project,
        requested_cwd: params.cwd,
        found_project: project.is_some(),
        mandates: bundle,
    }))
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
