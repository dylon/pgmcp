pub mod handlers;

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::config::Config;
use crate::context::SystemContext;
use crate::daemon_state::DaemonLifecycle;
use crate::db::DbClient;
use crate::embed::pool::QueryEmbedder;
use crate::llm::LlmExtractor;
use crate::llm::extractor_worker::DebounceMap;
use crate::reranker::Reranker;
use crate::stats::tracker::StatsTracker;

/// Shared state for REST API handlers.
#[derive(Clone)]
pub struct ApiState {
    pub db: Arc<dyn DbClient>,
    pub query_embedder: QueryEmbedder,
    pub config: Arc<ArcSwap<Config>>,
    /// Live in-process counters. The `/api/status` endpoint reads
    /// `http_mcp_sessions` and the model-scan counters from this.
    pub stats: Arc<StatsTracker>,
    /// Daemon lifecycle phase. The `/health` endpoint reads this for
    /// cheap 200/503 liveness without touching the database.
    /// `DaemonLifecycle` is `Clone` (internally Arc) so this is cheap.
    pub lifecycle: DaemonLifecycle,
    /// Memory-server Phase 4: LLM-driven salience extractor. `None` =
    /// extraction disabled (`[memory.extractor] backend = "disabled"`),
    /// in which case the session-observation pipeline runs Stage A
    /// only.
    pub llm_extractor: Arc<parking_lot::RwLock<Option<Arc<dyn LlmExtractor>>>>,
    /// Per-session debounce ledger for the Stage-B worker. Empty until
    /// the first observation; `extractor_worker` populates it.
    pub extractor_debounce: DebounceMap,
    /// SystemContext used by the A2A dispatcher to invoke MCP tools.
    /// Cheaply cloned (Arc-clone-per-field).
    pub system_ctx: SystemContext,
    /// Optional resident cross-encoder reranker for the `/api/search` hook.
    /// `Some` only when `[api] rerank_hook = true` (the BGE-reranker model is
    /// VRAM-exclusive with the Qwen3 extractor, so it's opt-in). The RRF
    /// dense+BM25 fusion runs regardless; this just adds a rerank stage.
    pub reranker: Arc<parking_lot::RwLock<Option<Arc<dyn Reranker>>>>,
}
