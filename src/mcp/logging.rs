//! MCP logging broadcaster for pushing log messages to connected clients.
//!
//! Uses atomic level filtering for lock-free reads from indexer threads,
//! and `ArcSwap<Vec<Peer>>` for lock-free peer list updates.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use arc_swap::ArcSwap;
use rmcp::RoleServer;
use rmcp::model::{LoggingLevel, LoggingMessageNotificationParam};
use rmcp::service::Peer;

/// Maps `LoggingLevel` to a u8 for atomic comparison.
fn level_to_u8(level: &LoggingLevel) -> u8 {
    match level {
        LoggingLevel::Debug => 0,
        LoggingLevel::Info => 1,
        LoggingLevel::Notice => 2,
        LoggingLevel::Warning => 3,
        LoggingLevel::Error => 4,
        LoggingLevel::Critical => 5,
        LoggingLevel::Alert => 6,
        LoggingLevel::Emergency => 7,
    }
}

/// Broadcasts log messages to connected MCP clients.
///
/// Thread-safe: can be shared across indexer threads via `Arc<LogBroadcaster>`.
pub struct LogBroadcaster {
    /// Current minimum log level (atomic for lock-free reads).
    level: AtomicU8,
    /// Connected peers. Uses ArcSwap for lock-free reads and COW updates.
    peers: ArcSwap<Vec<Peer<RoleServer>>>,
}

impl Default for LogBroadcaster {
    fn default() -> Self {
        Self::new()
    }
}

impl LogBroadcaster {
    /// Create a new broadcaster with default level `Info`.
    pub fn new() -> Self {
        Self {
            level: AtomicU8::new(level_to_u8(&LoggingLevel::Info)),
            peers: ArcSwap::from_pointee(Vec::new()),
        }
    }

    /// Set the minimum log level.
    pub fn set_level(&self, level: LoggingLevel) {
        self.level.store(level_to_u8(&level), Ordering::Release);
    }

    /// Hard cap on retained peers. A backstop against unbounded accumulation:
    /// `add_peer` had no counterpart removal, so every MCP client init (and every
    /// reconnect — frequent, especially across daemon restarts) permanently grew
    /// the fan-out Vec, and `log()` spawned a `param`-cloning notify task per peer
    /// per message to all of them — the 2026-07-08 tens-of-GB in-use balloon.
    const MAX_PEERS: usize = 64;

    /// Add a connected peer, opportunistically pruning any whose transport has
    /// since closed and evicting the oldest if the cap is exceeded. `rcu` makes the
    /// prune+append atomic against concurrent `add_peer`/`log` prunes (no lost
    /// update, unlike load→modify→store).
    pub fn add_peer(&self, peer: Peer<RoleServer>) {
        self.peers.rcu(|current| {
            let mut peers: Vec<Peer<RoleServer>> = current
                .iter()
                .filter(|p| !p.is_transport_closed())
                .cloned()
                .collect();
            peers.push(peer.clone());
            let overflow = peers.len().saturating_sub(Self::MAX_PEERS);
            if overflow > 0 {
                peers.drain(0..overflow);
            }
            Arc::new(peers)
        });
    }

    /// Number of retained peers (test/introspection).
    // Kept as a diagnostic accessor for the log-broadcaster peer set; has no
    // production caller yet, so `#[allow(dead_code)]` keeps the intended API
    // without tripping the `-D warnings` gate (pre-existing since f5b13de).
    #[allow(dead_code)]
    pub fn peer_count(&self) -> usize {
        self.peers.load().len()
    }

    /// Check if a message at the given level would be logged.
    pub fn should_log(&self, level: &LoggingLevel) -> bool {
        level_to_u8(level) >= self.level.load(Ordering::Acquire)
    }

    /// Send a log message to all connected peers (if level passes the filter).
    ///
    /// Spawns a tokio task per peer to avoid blocking the caller.
    /// Peers that have disconnected are silently skipped.
    pub fn log(&self, level: LoggingLevel, logger: &str, data: serde_json::Value) {
        if !self.should_log(&level) {
            return;
        }

        let peers = self.peers.load();
        if peers.is_empty() {
            return;
        }

        // Prune peers whose transport has closed so the fan-out list cannot grow
        // without bound (there is no remove-on-disconnect hook). Only pay the rcu
        // COW when there is actually something to drop.
        if peers.iter().any(|p| p.is_transport_closed()) {
            self.peers.rcu(|current| {
                Arc::new(
                    current
                        .iter()
                        .filter(|p| !p.is_transport_closed())
                        .cloned()
                        .collect::<Vec<Peer<RoleServer>>>(),
                )
            });
        }

        let param = LoggingMessageNotificationParam::new(level, data).with_logger(logger);

        for peer in peers.iter() {
            if peer.is_transport_closed() {
                continue;
            }
            let peer = peer.clone();
            let param = param.clone();
            tokio::spawn(async move {
                // Bound the notify: a stuck / dead-but-not-yet-closed peer must not
                // hold its `param` clone (and pin memory) indefinitely — the other
                // half of the balloon. 5 s is generous for a live client.
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    peer.notify_logging_message(param),
                )
                .await;
            });
        }
    }
}
