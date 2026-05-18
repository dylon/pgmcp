pub mod handlers;

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::config::Config;
use crate::daemon_state::DaemonLifecycle;
use crate::db::DbClient;
use crate::embed::pool::QueryEmbedder;
use crate::llm::LlmExtractor;
use crate::llm::extractor_worker::DebounceMap;
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
    pub llm_extractor: Option<Arc<dyn LlmExtractor>>,
    /// Per-session debounce ledger for the Stage-B worker. Empty until
    /// the first observation; `extractor_worker` populates it.
    pub extractor_debounce: DebounceMap,
}
