//! Closed vocabularies + IR for the ordered synchronization skeleton (shadow-ASR).
//!
//! `symbol_effects` records the *unordered* set of concurrency effects a symbol
//! carries (e.g. `lock_acquire`, `channel_send`), but static deadlock detection
//! and Petri-net bottleneck analysis need the ORDERED, scope-nested sequence of
//! lock acquire/release and channel send/recv operations, with best-effort
//! resource identity. `LanguageBackend::extract_sync_ops` walks each
//! function/contract body and emits an ordered [`FunctionSyncOps`]; the
//! symbol-extraction cron persists it into `sync_ops` (migration
//! `v21_sync_ops`), keyed to `file_symbols` by the same `(name, start_line)`
//! identity the metrics/dataflow passes use.
//!
//! Per ADR-003 each vocabulary column is `TEXT` + a `CHECK` built from a closed
//! Rust enum via its `*_sql_in_list()`, with a `#[cfg(test)]` golden test
//! pinning the set — the same idiom as
//! [`crate::parsing::resolution_kind::ResolutionKind`]. Emitting the enum's
//! `as_db_str()` from the extractor (not string literals) keeps writer ⇄ CHECK
//! in lockstep at compile time.
//!
//! Kept self-contained (no `crate::tracker` / DB dependency) so the parsing
//! layer stays leaf-level.
//!
//! **Note on call sites.** The lock-order analyzer needs call sites interleaved
//! with acquires in program order to compute the held-set at each call. We do
//! NOT store a `Call` op per call site here (that would duplicate the entire
//! resolved call graph into `sync_ops`); instead the analyzer merge-sorts a
//! symbol's `sync_ops` (by `line`) with its outgoing `symbol_references` (by
//! `source_line`, which `symbol_references` already stores). So `SyncOpKind`
//! carries only genuine synchronization operations.

use serde::{Deserialize, Serialize};

/// A single synchronization operation in program order within a function body.
/// `seq` is 0-based and dense within the owning symbol. Resource identity is
/// best-effort (`resource_key` is `None` when the receiver is an arbitrary
/// expression); `resource_confidence` records which [`ResourceConfidence`] tier
/// produced the key so the analysis layer can discount weak edges.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncOp {
    pub seq: u32,
    pub op_kind: SyncOpKind,
    pub resource_kind: ResourceKind,
    pub paradigm: SyncParadigm,
    /// Best-effort static identity (normalized access path / channel name);
    /// `None` when unknowable.
    pub resource_key: Option<String>,
    /// Confidence in `resource_key`, in `[0.0, 1.0]` (see [`ResourceConfidence`]).
    pub resource_confidence: f32,
    /// Lexical block depth relative to the symbol body (body == 0).
    pub nesting_depth: u32,
    /// Pairs an acquire with its synthesized/explicit release (same `guard_id`);
    /// `None` for unscoped ops (channel send/recv, escaping guards).
    pub guard_id: Option<u32>,
    /// 1-based source line of the op.
    pub line: u32,
}

/// One function/contract's ordered synchronization skeleton. `function` +
/// `start_line` are the join key to `file_symbols` (the same identity the
/// metrics and dataflow passes use). Mirrors
/// [`crate::parsing::dataflow::FunctionDataflow`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FunctionSyncOps {
    pub function: String,
    pub start_line: u32,
    pub end_line: u32,
    /// In program order; `seq` is 0-based and dense.
    pub ops: Vec<SyncOp>,
}

/// What a [`SyncOp`] does. The acquire family ([`SyncOpKind::is_acquire`]) and
/// `Release` drive the lock-order graph; the message family
/// (`Send`/`Recv`/`Spawn`/`Select`) drives the Petri-net channel analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncOpKind {
    /// Generic exclusive acquire (`Mutex::lock`, `Mutex::try_lock`).
    Acquire,
    /// Shared/read acquire (`RwLock::read`).
    AcquireRead,
    /// Exclusive/write acquire (`RwLock::write`) — distinct from `Acquire` so
    /// the lock-order graph can apply the rwlock RR/WR/WW refinement.
    AcquireWrite,
    /// Release of a guard — synthesized at end-of-scope for RAII guards, or
    /// explicit (`drop(guard)` / `unlock`).
    Release,
    /// Channel send (Rholang `!`, Rust `tx.send`).
    Send,
    /// Persistent/replicated send (Rholang `!!`).
    SendPersistent,
    /// Linear/once receive (Rholang `<-`, Rust `rx.recv`).
    Recv,
    /// Persistent/replicated receive (Rholang `<=` / `contract`) — a receiver
    /// that stays armed; modeled as a self-looping Petri transition.
    RecvPersistent,
    /// Process/thread/task spawn (Rholang `|`, `thread::spawn`, `tokio::spawn`).
    Spawn,
    /// Await suspension point (Rust `.await`).
    Await,
    /// Non-deterministic choice (Rholang `select`, `tokio::select!`).
    Select,
}

impl SyncOpKind {
    /// Canonical ordering; the source of the DB CHECK vocabulary.
    pub const ALL: &'static [SyncOpKind] = &[
        Self::Acquire,
        Self::AcquireRead,
        Self::AcquireWrite,
        Self::Release,
        Self::Send,
        Self::SendPersistent,
        Self::Recv,
        Self::RecvPersistent,
        Self::Spawn,
        Self::Await,
        Self::Select,
    ];

    /// Stable string stored in `sync_ops.op_kind`.
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Acquire => "acquire",
            Self::AcquireRead => "acquire_read",
            Self::AcquireWrite => "acquire_write",
            Self::Release => "release",
            Self::Send => "send",
            Self::SendPersistent => "send_persistent",
            Self::Recv => "recv",
            Self::RecvPersistent => "recv_persistent",
            Self::Spawn => "spawn",
            Self::Await => "await",
            Self::Select => "select",
        }
    }

    /// Parse a DB string back into the enum. Part of the closed surface; read by
    /// the analysis layer when it reconstructs the skeleton from `sync_ops`.
    pub fn from_db_str(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_db_str() == s)
    }

    /// True for the acquire family — the lock-order graph only edges between
    /// these (and tracks `Release` to pop the held-set).
    pub fn is_acquire(self) -> bool {
        matches!(self, Self::Acquire | Self::AcquireRead | Self::AcquireWrite)
    }

    /// True for the message-passing family — consumed by the Petri-net builder.
    pub fn is_message(self) -> bool {
        matches!(
            self,
            Self::Send
                | Self::SendPersistent
                | Self::Recv
                | Self::RecvPersistent
                | Self::Spawn
                | Self::Select
        )
    }
}

/// What kind of resource a [`SyncOp`] touches. `Unknown` is a real member (every
/// op touches *some* resource even when identity is unknowable), so the column
/// is `NOT NULL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Mutex,
    Rwlock,
    Channel,
    Task,
    Atomic,
    Condvar,
    Semaphore,
    Once,
    Unknown,
}

impl ResourceKind {
    pub const ALL: &'static [ResourceKind] = &[
        Self::Mutex,
        Self::Rwlock,
        Self::Channel,
        Self::Task,
        Self::Atomic,
        Self::Condvar,
        Self::Semaphore,
        Self::Once,
        Self::Unknown,
    ];

    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Mutex => "mutex",
            Self::Rwlock => "rwlock",
            Self::Channel => "channel",
            Self::Task => "task",
            Self::Atomic => "atomic",
            Self::Condvar => "condvar",
            Self::Semaphore => "semaphore",
            Self::Once => "once",
            Self::Unknown => "unknown",
        }
    }

    #[allow(dead_code)] // closed-vocab surface; exercised by the parse roundtrip test.
    pub fn from_db_str(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_db_str() == s)
    }

    /// True for the shared-memory lock family — these resources become
    /// `lock_resource` nodes in the unified graph and participate in the
    /// lock-order graph.
    #[allow(dead_code)] // reserved API (lock-node classification); SQL arms inline the list.
    pub fn is_lock(self) -> bool {
        matches!(
            self,
            Self::Mutex | Self::Rwlock | Self::Condvar | Self::Semaphore | Self::Once
        )
    }
}

/// Which analysis paradigm owns an op — lets the analysis layer cheaply split
/// the stream into the lock-order half and the Petri-net half (the
/// best-per-paradigm portfolio).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncParadigm {
    Lock,
    Message,
}

impl SyncParadigm {
    pub const ALL: &'static [SyncParadigm] = &[Self::Lock, Self::Message];

    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Lock => "lock",
            Self::Message => "message",
        }
    }

    #[allow(dead_code)] // closed-vocab surface; exercised by the parse roundtrip test.
    pub fn from_db_str(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_db_str() == s)
    }
}

/// Confidence tiers for [`SyncOp::resource_key`], mirroring
/// [`crate::parsing::resolution_kind::ResolutionKind`]'s confidence ladder.
/// Producer-side only (stored as a `REAL`, not a closed-vocab TEXT column), so
/// it carries no `sql_in_list`/CHECK — just the single source of truth for the
/// numeric confidence each extraction tier writes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResourceConfidence {
    /// Receiver is a stable access path rooted at `self`/a binding
    /// (`self.state.lock()` → `self.state`).
    FieldPath,
    /// Rholang channel name or a named `new`-bound channel (first-class).
    ChannelName,
    /// Receiver is a local variable with a single visible `let` def.
    LocalBinding,
    /// Neither path nor channel-name recoverable (arbitrary receiver
    /// expression). (Type-aware keying — a `type:Mutex` mid-tier — is reserved
    /// for a v2 pass that cross-checks declared types; v1 does not emit it.)
    Unknown,
}

impl ResourceConfidence {
    /// The numeric confidence written to `sync_ops.resource_confidence`.
    pub fn value(self) -> f32 {
        match self {
            Self::FieldPath => 0.9,
            Self::ChannelName => 0.85,
            Self::LocalBinding => 0.8,
            Self::Unknown => 0.2,
        }
    }
}

/// SQL `IN (...)` value list for `sync_ops.op_kind` — single source of truth
/// shared with the `chk_sync_ops_op_kind` constraint (`v21_sync_ops`).
pub fn op_kind_sql_in_list() -> String {
    SyncOpKind::ALL
        .iter()
        .map(|k| format!("'{}'", k.as_db_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// SQL `IN (...)` value list for `sync_ops.resource_kind`.
pub fn resource_kind_sql_in_list() -> String {
    ResourceKind::ALL
        .iter()
        .map(|k| format!("'{}'", k.as_db_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// SQL `IN (...)` value list for `sync_ops.paradigm`.
pub fn paradigm_sql_in_list() -> String {
    SyncParadigm::ALL
        .iter()
        .map(|k| format!("'{}'", k.as_db_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn sync_op_kind_vocabulary_is_pinned() {
        let got: HashSet<&str> = SyncOpKind::ALL.iter().map(|k| k.as_db_str()).collect();
        let expected: HashSet<&str> = [
            "acquire",
            "acquire_read",
            "acquire_write",
            "release",
            "send",
            "send_persistent",
            "recv",
            "recv_persistent",
            "spawn",
            "await",
            "select",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            got, expected,
            "SyncOpKind drifted — update the v21_sync_ops CHECK and extract_sync_ops together"
        );
        assert_eq!(SyncOpKind::ALL.len(), 11);
        assert_eq!(got.len(), 11, "duplicate as_db_str() in SyncOpKind");
    }

    #[test]
    fn resource_kind_vocabulary_is_pinned() {
        let got: HashSet<&str> = ResourceKind::ALL.iter().map(|k| k.as_db_str()).collect();
        let expected: HashSet<&str> = [
            "mutex",
            "rwlock",
            "channel",
            "task",
            "atomic",
            "condvar",
            "semaphore",
            "once",
            "unknown",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            got, expected,
            "ResourceKind drifted — update the v21_sync_ops CHECK and extract_sync_ops together"
        );
        assert_eq!(ResourceKind::ALL.len(), 9);
        assert_eq!(got.len(), 9, "duplicate as_db_str() in ResourceKind");
    }

    #[test]
    fn sync_paradigm_vocabulary_is_pinned() {
        let got: HashSet<&str> = SyncParadigm::ALL.iter().map(|k| k.as_db_str()).collect();
        let expected: HashSet<&str> = ["lock", "message"].into_iter().collect();
        assert_eq!(got, expected, "SyncParadigm drifted from pinned set");
        assert_eq!(SyncParadigm::ALL.len(), 2);
    }

    #[test]
    fn parse_roundtrips_for_all() {
        for k in SyncOpKind::ALL {
            assert_eq!(SyncOpKind::from_db_str(k.as_db_str()), Some(*k));
        }
        for k in ResourceKind::ALL {
            assert_eq!(ResourceKind::from_db_str(k.as_db_str()), Some(*k));
        }
        for k in SyncParadigm::ALL {
            assert_eq!(SyncParadigm::from_db_str(k.as_db_str()), Some(*k));
        }
        assert_eq!(SyncOpKind::from_db_str("nonsense"), None);
        assert_eq!(ResourceKind::from_db_str("nonsense"), None);
        assert_eq!(SyncParadigm::from_db_str("nonsense"), None);
    }

    #[test]
    fn sql_in_lists_quote_every_value() {
        let op = op_kind_sql_in_list();
        assert!(op.starts_with("'acquire'"), "got: {op}");
        assert!(op.contains("'recv_persistent'"));
        assert_eq!(op.matches('\'').count(), SyncOpKind::ALL.len() * 2);
        assert_eq!(op.matches(',').count(), SyncOpKind::ALL.len() - 1);

        let rk = resource_kind_sql_in_list();
        assert!(rk.contains("'mutex'") && rk.contains("'channel'") && rk.contains("'unknown'"));
        assert_eq!(rk.matches('\'').count(), ResourceKind::ALL.len() * 2);

        let pg = paradigm_sql_in_list();
        assert!(pg.contains("'lock'") && pg.contains("'message'"));
        assert_eq!(pg.matches(',').count(), SyncParadigm::ALL.len() - 1);
    }

    #[test]
    fn op_kind_family_partition() {
        // Acquire family and message family are disjoint; Release is neither.
        for k in SyncOpKind::ALL {
            assert!(
                !(k.is_acquire() && k.is_message()),
                "{k:?} cannot be both acquire and message"
            );
        }
        assert!(SyncOpKind::Acquire.is_acquire());
        assert!(SyncOpKind::AcquireWrite.is_acquire());
        assert!(!SyncOpKind::Release.is_acquire() && !SyncOpKind::Release.is_message());
        assert!(SyncOpKind::Send.is_message() && SyncOpKind::Spawn.is_message());
    }

    #[test]
    fn resource_confidence_within_bounds_and_ordered() {
        use ResourceConfidence::*;
        for c in [FieldPath, ChannelName, LocalBinding, Unknown] {
            assert!((0.0..=1.0).contains(&c.value()), "{c:?} out of [0,1]");
        }
        assert!(FieldPath.value() > LocalBinding.value());
        assert!(LocalBinding.value() > Unknown.value());
    }
}
