//! `SystemContext` — the dependency bundle for the daemon's stateful subsystems.
//!
//! Replaces the 6-parameter `McpServer::new` and the 9-parameter
//! `start_indexing` signatures with a single `SystemContext` argument that
//! owns shared singleton state:
//!
//! - `db: Arc<dyn DbClient>` — DbClient trait object (production: PgPool;
//!   tests: MockDbClient).
//! - `embed: EmbedSource` — query-time embedding source (Pool variant in
//!   daemon mode, Lazy variant in CLI mode). Phase 7 will further abstract
//!   this behind an `EmbeddingBackend` trait.
//! - `stats: Arc<StatsTracker>` — atomic counters (uptime, MCP request
//!   counts, cron run counts, etc.).
//! - `config: Arc<ArcSwap<Config>>` — hot-swappable config snapshot.
//! - `log_broadcaster: Arc<LogBroadcaster>` — pushes log notifications to
//!   connected MCP clients.
//! - `task_store: Arc<TaskStore>` — tracks long-running operations
//!   (e.g. reindex, surfaced via the MCP `tasks` capability).
//!
//! `SystemContext` is `Clone`-via-`Arc::clone`-on-each-field — cheap to
//! pass around. The trait fields hold real shared state; cloning the
//! context never deep-copies anything.

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::config::Config;
use crate::daemon_state::DaemonLifecycle;
use crate::db::DbClient;
use crate::embed::EmbedSource;
use crate::llm::LlmExtractor;
use crate::mcp::logging::LogBroadcaster;
use crate::mcp::tasks::TaskStore;
use crate::stats::tracker::StatsTracker;

/// Bundled dependencies shared across the daemon's stateful subsystems.
#[derive(Clone)]
pub struct SystemContext {
    db: Arc<dyn DbClient>,
    embed: EmbedSource,
    stats: Arc<StatsTracker>,
    config: Arc<ArcSwap<Config>>,
    log_broadcaster: Arc<LogBroadcaster>,
    task_store: Arc<TaskStore>,
    /// Daemon lifecycle phase. In daemon mode this is the same handle the
    /// scanner/cron/MCP all read; CLI tool invocations construct a fresh
    /// `DaemonLifecycle` and immediately advance it to `Ready` so trait
    /// callers get a sensible answer when the CLI binary calls a tool
    /// directly without a running daemon. `DaemonLifecycle` is already
    /// internally `Arc<AtomicU8>` + `Arc<Subject>`, so we hold by value
    /// and rely on its derived `Clone` for cheap propagation.
    lifecycle: DaemonLifecycle,
    /// Memory-server Phase 4+5 LLM extractor. `None` = extraction
    /// disabled (operator hasn't opted in or construction failed). MCP
    /// tools that need the extractor (currently `memory_reflect`)
    /// refuse cleanly when this is `None`.
    llm_extractor: Option<Arc<dyn LlmExtractor>>,
    /// Serializes any caller that performs a mass-delete reindex (the
    /// MCP `reindex` tool, future cron equivalents, etc.). Held for the
    /// duration of the destructive operation so two reindexes cannot
    /// race the live indexer pool and each other. Acquired non-blocking
    /// via `try_lock`; a held lock surfaces as a `Conflict`-style error
    /// to the caller rather than queueing.
    reindex_lock: Arc<tokio::sync::Mutex<()>>,
}

impl SystemContext {
    /// Build the production context. Called from `main.rs::run_server`
    /// after each component is constructed; passed by-clone (Arc-clone on
    /// every field) into `McpServer::new`, `start_indexing`, the cron
    /// scheduler, and the REST API.
    #[allow(clippy::too_many_arguments)]
    pub fn production(
        db: Arc<dyn DbClient>,
        embed: EmbedSource,
        stats: Arc<StatsTracker>,
        config: Arc<ArcSwap<Config>>,
        log_broadcaster: Arc<LogBroadcaster>,
        task_store: Arc<TaskStore>,
        lifecycle: DaemonLifecycle,
    ) -> Self {
        Self {
            db,
            embed,
            stats,
            config,
            log_broadcaster,
            task_store,
            lifecycle,
            llm_extractor: None,
            reindex_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Variant of `production` that attaches an LLM extractor. The
    /// daemon constructs the extractor in `cli/daemon.rs::serve_daemon`
    /// and threads it through here.
    #[allow(clippy::too_many_arguments)]
    pub fn production_with_extractor(
        db: Arc<dyn DbClient>,
        embed: EmbedSource,
        stats: Arc<StatsTracker>,
        config: Arc<ArcSwap<Config>>,
        log_broadcaster: Arc<LogBroadcaster>,
        task_store: Arc<TaskStore>,
        lifecycle: DaemonLifecycle,
        llm_extractor: Option<Arc<dyn LlmExtractor>>,
    ) -> Self {
        Self {
            db,
            embed,
            stats,
            config,
            log_broadcaster,
            task_store,
            lifecycle,
            llm_extractor,
            reindex_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub fn db(&self) -> &Arc<dyn DbClient> {
        &self.db
    }

    pub fn embed(&self) -> &EmbedSource {
        &self.embed
    }

    pub fn stats(&self) -> &Arc<StatsTracker> {
        &self.stats
    }

    /// Same as `stats()` but returns the cloneable Arc directly for
    /// callers that spawn tasks needing ownership.
    pub fn stats_arc(&self) -> &Arc<StatsTracker> {
        &self.stats
    }

    pub fn config(&self) -> &Arc<ArcSwap<Config>> {
        &self.config
    }

    /// The reindex serialization lock — see field-level docstring.
    pub fn reindex_lock(&self) -> &Arc<tokio::sync::Mutex<()>> {
        &self.reindex_lock
    }

    pub fn log_broadcaster(&self) -> &Arc<LogBroadcaster> {
        &self.log_broadcaster
    }

    pub fn task_store(&self) -> &Arc<TaskStore> {
        &self.task_store
    }

    pub fn lifecycle(&self) -> &DaemonLifecycle {
        &self.lifecycle
    }

    /// Returns the optional LLM extractor handle. `None` when the
    /// operator hasn't opted in (or construction failed at daemon
    /// startup).
    pub fn llm_extractor(&self) -> Option<&Arc<dyn LlmExtractor>> {
        self.llm_extractor.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time check that SystemContext is Send + Sync (required for
    /// it to cross tokio::spawn boundaries).
    fn _assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn context_is_send_and_sync() {
        _assert_send_sync::<SystemContext>();
    }
}
