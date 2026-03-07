//! MCP logging broadcaster for pushing log messages to connected clients.
//!
//! Uses atomic level filtering for lock-free reads from indexer threads,
//! and `ArcSwap<Vec<Peer>>` for lock-free peer list updates.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use rmcp::model::{LoggingLevel, LoggingMessageNotificationParam};
use rmcp::service::Peer;
use rmcp::RoleServer;

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

    /// Add a connected peer.
    pub fn add_peer(&self, peer: Peer<RoleServer>) {
        let guard = self.peers.load();
        let mut peers = guard.to_vec();
        peers.push(peer);
        self.peers.store(Arc::new(peers));
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

        let param = LoggingMessageNotificationParam::new(level, data)
            .with_logger(logger);

        for peer in peers.iter() {
            if peer.is_transport_closed() {
                continue;
            }
            let peer = peer.clone();
            let param = param.clone();
            tokio::spawn(async move {
                let _ = peer.notify_logging_message(param).await;
            });
        }
    }
}
