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
use crate::db::DbClient;
use crate::embed::EmbedSource;
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
    ) -> Self {
        Self {
            db,
            embed,
            stats,
            config,
            log_broadcaster,
            task_store,
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

    pub fn config(&self) -> &Arc<ArcSwap<Config>> {
        &self.config
    }

    pub fn log_broadcaster(&self) -> &Arc<LogBroadcaster> {
        &self.log_broadcaster
    }

    pub fn task_store(&self) -> &Arc<TaskStore> {
        &self.task_store
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
