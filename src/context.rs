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

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use arc_swap::ArcSwap;
use dashmap::DashMap;

use crate::config::Config;
use crate::daemon_state::DaemonLifecycle;
use crate::db::DbClient;
use crate::embed::EmbedSource;
use crate::fuzzy::phonetic::PgmcpPhonetics;
use crate::llm::LlmExtractor;
use crate::mcp::client_profile::ClientProfileRegistry;
use crate::mcp::logging::LogBroadcaster;
use crate::mcp::tasks::TaskStore;
use crate::stats::tracker::StatsTracker;

/// Per-project `PgmcpPhonetics` registry. Keyed by `project_root`
/// (the same `PathBuf` `event_processor.rs` uses for
/// `project_overrides_for_filter`), the installation thread is the
/// `.pgmcp.toml`-change handler in `event_processor.rs`. Readers
/// are the three phonetic MCP tools; writers are the
/// `install_phonetics_for_project` helper (also drop-removed on
/// `.pgmcp.toml` removal events).
pub type PhoneticsRegistry = Arc<DashMap<PathBuf, Arc<PgmcpPhonetics>>>;

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
    /// Memory-server Phase 4+5 LLM extractor. Empty = extraction disabled
    /// (operator hasn't opted in or construction failed) OR still loading — the
    /// daemon builds it in the background and hot-swaps it in, so readers see
    /// `None` only briefly during startup. MCP tools that need the extractor
    /// (currently `memory_reflect`) refuse cleanly while it is absent.
    llm_extractor: Arc<parking_lot::RwLock<Option<Arc<dyn LlmExtractor>>>>,
    /// Serializes any caller that performs a mass-delete reindex (the
    /// MCP `reindex` tool, future cron equivalents, etc.). Held for the
    /// duration of the destructive operation so two reindexes cannot
    /// race the live indexer pool and each other. Acquired non-blocking
    /// via `try_lock`; a held lock surfaces as a `Conflict`-style error
    /// to the caller rather than queueing.
    reindex_lock: Arc<tokio::sync::Mutex<()>>,
    /// P14.4 — per-project `Arc<PgmcpPhonetics>` registry. Populated
    /// by `event_processor.rs`'s `.pgmcp.toml` change handler when
    /// the project's `ProjectOverride.phonetics.rules_path` is set.
    /// Empty in tests / CLI mode (the `phonetics_for` lookup falls
    /// back to `default_phonetics` for unknown projects).
    phonetics: PhoneticsRegistry,
    /// P14.4 — lazy-initialized embedded-English `PgmcpPhonetics`
    /// used as the default when `phonetics_for(...)` cannot find a
    /// per-project entry (no `project` param supplied, or the
    /// project's `.pgmcp.toml` has no `phonetics.rules_path`).
    default_phonetics: Arc<OnceLock<Arc<PgmcpPhonetics>>>,
    /// Client-profile registry (per-client output format + tool-description
    /// overrides), loaded once lazily on first access. Backs the per-client
    /// `list_tools` description overrides and the `pgmcp_client_profile` tool.
    client_profiles: Arc<OnceLock<Arc<ClientProfileRegistry>>>,
    /// Bounded cache of open per-project fuzzy-trie handles (symbols + paths).
    /// Lets the fuzzy MCP tools reuse one `PersistentARTrieChar` handle across
    /// calls instead of opening one (and spawning three daemon threads) per
    /// query; entries self-invalidate when the `fuzzy-sync` cron rewrites the
    /// on-disk `.artrie` (mtime change). See `crate::fuzzy::cache::FuzzyCache`.
    fuzzy_cache: Arc<crate::fuzzy::cache::FuzzyCache>,
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
            llm_extractor: Arc::new(parking_lot::RwLock::new(None)),
            reindex_lock: Arc::new(tokio::sync::Mutex::new(())),
            phonetics: Arc::new(DashMap::new()),
            default_phonetics: Arc::new(OnceLock::new()),
            client_profiles: Arc::new(OnceLock::new()),
            fuzzy_cache: Arc::new(crate::fuzzy::cache::FuzzyCache::new()),
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
        llm_extractor: Arc<parking_lot::RwLock<Option<Arc<dyn LlmExtractor>>>>,
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
            phonetics: Arc::new(DashMap::new()),
            default_phonetics: Arc::new(OnceLock::new()),
            client_profiles: Arc::new(OnceLock::new()),
            fuzzy_cache: Arc::new(crate::fuzzy::cache::FuzzyCache::new()),
        }
    }

    /// Shared, bounded cache of open per-project fuzzy-trie handles (see
    /// [`crate::fuzzy::cache::FuzzyCache`]). The fuzzy MCP query tools use it to
    /// reuse trie handles across calls rather than reopening one per query.
    pub fn fuzzy_cache(&self) -> &crate::fuzzy::cache::FuzzyCache {
        &self.fuzzy_cache
    }

    /// P14.4 — clone the per-project phonetics registry Arc. The
    /// daemon's `start_event_processing` threads this through so the
    /// `.pgmcp.toml`-change handler can install / tear down
    /// `PgmcpPhonetics` watchers as projects come and go.
    pub fn phonetics_registry(&self) -> &PhoneticsRegistry {
        &self.phonetics
    }

    /// P14.4 — resolve a `PgmcpPhonetics` handle for an MCP tool
    /// call. Tries the registry by trailing path-segment match
    /// against `project`; if no per-project entry exists (or
    /// `project` is `None`), returns the embedded-English default
    /// (lazy-initialized).
    pub fn phonetics_for(&self, project: Option<&str>) -> Arc<PgmcpPhonetics> {
        if let Some(name) = project {
            // Walk the registry. The key is the project root
            // `PathBuf`; `file_name()` gives the trailing segment.
            // Equality on segment matches the convention used by
            // `event_processor.rs`'s `project_overrides_for_filter`
            // (also keyed by `PathBuf` but looked up by full path
            // via the indexer). We do segment match here because the
            // MCP `project` param is the human-readable name, not a
            // full path.
            for entry in self.phonetics.iter() {
                if entry
                    .key()
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| s == name)
                    .unwrap_or(false)
                {
                    return Arc::clone(entry.value());
                }
            }
        }
        Arc::clone(
            self.default_phonetics
                .get_or_init(|| Arc::new(PgmcpPhonetics::default_english())),
        )
    }

    /// The client-profile registry, loaded once lazily (built-ins layered with
    /// `assets/client_profiles.toml`). Backs per-client `list_tools` description
    /// overrides and the `pgmcp_client_profile` tool. Deref-coerces the cached
    /// `&Arc<ClientProfileRegistry>` to `&ClientProfileRegistry`.
    pub fn client_profiles(&self) -> &ClientProfileRegistry {
        self.client_profiles.get_or_init(|| {
            Arc::new(ClientProfileRegistry::load_or_builtin(
                &crate::mcp::tools::tool_client_profile::profiles_path(),
            ))
        })
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

    /// Returns the current LLM extractor handle (owned `Arc`), or `None` when
    /// the operator hasn't opted in, construction failed, or it is still loading
    /// in the background at startup. Cheap atomic load; call per use rather than
    /// caching so a late background swap-in is picked up.
    pub fn llm_extractor(&self) -> Option<Arc<dyn LlmExtractor>> {
        self.llm_extractor.read().clone()
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
