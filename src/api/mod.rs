pub mod audit;
pub mod auth;
pub mod db_browser;
pub mod experiments;
pub mod handlers;
pub mod logs;
pub mod mandates_write;
pub mod metrics;
/// Shared plumbing (writes kill-switch, pool accessor, audit IP, DB-error map)
/// for the token-gated operator write surface. Private to `api`; the two write
/// modules reach it via `super::operator`.
mod operator;
pub mod resources;
pub mod work_items_write;

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::config::Config;
use crate::context::SystemContext;
use crate::daemon_state::DaemonLifecycle;
use crate::db::DbClient;
use crate::embed::pool::QueryEmbedder;
use crate::health::Outbox;
use crate::llm::LlmExtractor;
use crate::llm::extractor_worker::DebounceMap;
use crate::reranker::Reranker;
use crate::stats::tracker::StatsTracker;

/// Shared state for REST API handlers.
#[derive(Clone)]
pub struct ApiState {
    pub db: Arc<dyn DbClient>,
    /// Fleet-wide ALL-STOP flag (ADR-016 E8). When true, the A2A dispatcher
    /// refuses new tasks and aborts in-flight ones at the next round boundary.
    /// Mirrored durably in `system_control`; restored at startup.
    pub halted: Arc<std::sync::atomic::AtomicBool>,
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
    /// Durable ephemeral-event outbox (src/health). `Some` when `[outbox]
    /// enabled` and its spool directory is writable. The session-observe and
    /// client-file-event handlers append the raw request here when the breaker
    /// reports the DB down; the prober replays them on recovery.
    pub outbox: Option<Arc<Outbox>>,
}
