//! Closed vocabularies for client file-attribution events (Phase 2). Per
//! ADR-003 each is a `TEXT` column + a `CHECK` built from a closed Rust enum via
//! a `sql_in_list` helper, with a `#[cfg(test)]` golden test pinning the set —
//! the same idiom as [`crate::tracker::severity`]. Shared by the `v26`
//! `client_file_events` migration (the CHECK source of truth), the
//! `/api/client/file_event` ingestion handler, and the eBPF/`proc_fd` capture
//! paths.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::tracker::kind::join_quoted;

/// What a client did to a file. Stored in `client_file_events.op`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOp {
    /// Opened for reading / inspected (an `open()` without a subsequent write).
    Open,
    /// Read content — a `Read`-tool call or a `read()` syscall.
    Read,
    /// Wrote or created content — a `Write`-tool call or `write()` syscall.
    Write,
    /// Edited in place — an `Edit` / `NotebookEdit`-tool call.
    Edit,
    /// Closed a writable fd — the `FAN_CLOSE_WRITE`-style "edit flushed" signal.
    Close,
}

impl FileOp {
    /// Canonical set; the source of the DB CHECK vocabulary.
    pub const ALL: &'static [FileOp] =
        &[Self::Open, Self::Read, Self::Write, Self::Edit, Self::Close];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Read => "read",
            Self::Write => "write",
            Self::Edit => "edit",
            Self::Close => "close",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|o| o.as_str() == s)
    }

    /// Whether this op is a *modification* — edit-weighted attribution ranks
    /// these above reads in `client_project_matrix` (which weights in SQL, so this
    /// classifier has no internal caller yet; a deliberate API member exercised by
    /// the unit tests, `#[allow(dead_code)]` like `Severity::rank`).
    #[allow(dead_code)]
    pub fn is_write(self) -> bool {
        matches!(self, Self::Write | Self::Edit | Self::Close)
    }
}

/// SQL `IN (...)` value list for the `client_file_events_op_check` constraint.
pub fn op_sql_in_list() -> String {
    join_quoted(FileOp::ALL.iter().map(|o| o.as_str()))
}

/// Where a file event was captured. Stored in `client_file_events.source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileEventSource {
    /// The Claude Code `PostToolUse` hook — precise, zero-privilege (Phase 2A).
    ClientHook,
    /// eBPF syscall tracing filtered to client PIDs — client-agnostic (Phase 2B).
    Ebpf,
    /// `/proc/<pid>/fd` sampling on the liveness tick — best-effort supplement.
    ProcFd,
    /// eBPF syscall tracing filtered by **cgroup id** — captures the agent's
    /// whole process *subtree* (`cargo`→`rustc`→`rg`, …), because cgroup
    /// membership is inherited across `fork`/`exec`. The PID-exact `Ebpf` source
    /// sees only the agent process itself; this one closes the subprocess gap
    /// (Phase 2C, ADR-022).
    EbpfCgroup,
    /// Unprivileged `LD_PRELOAD` libc interposition in the agent's process
    /// *subtree* — captures file *edits* by spawned subprocesses (`cargo`/`rustc`/
    /// …) with **no caps, no root, no cgroup**; attribution rests on `agent_id`
    /// set by the launch wrapper. Complements `EbpfCgroup`; blind to statically
    /// linked / setuid children (Phase 2D, ADR-022).
    Preload,
}

impl FileEventSource {
    /// Canonical set; the source of the DB CHECK vocabulary.
    pub const ALL: &'static [FileEventSource] = &[
        Self::ClientHook,
        Self::Ebpf,
        Self::ProcFd,
        Self::EbpfCgroup,
        Self::Preload,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClientHook => "client_hook",
            Self::Ebpf => "ebpf",
            Self::ProcFd => "proc_fd",
            Self::EbpfCgroup => "ebpf_cgroup",
            Self::Preload => "preload",
        }
    }

    /// Inverse of [`as_str`](Self::as_str) — a deliberate closed-vocab API member
    /// (ADR-003 idiom) exercised by the golden tests; the capture paths write the
    /// source literal directly, so there is no internal caller yet.
    #[allow(dead_code)]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s2| s2.as_str() == s)
    }
}

/// SQL `IN (...)` value list for the `client_file_events_source_check` constraint.
pub fn source_sql_in_list() -> String {
    join_quoted(FileEventSource::ALL.iter().map(|s| s.as_str()))
}

/// One unified file-touch event flowing through the reactive ingestion stream
/// (ADR-022). **Every** capture source — the `POST /api/client/file_event`
/// handler, the eBPF cgroup probe, the `/proc/<pid>/fd` sampler — emits this into
/// a single bounded [`crate::reactive::subject::Subject`]; one batched writer is
/// the sole consumer that resolves `project_id`/`file_id` and inserts a
/// `client_file_events` row.
///
/// Project/file resolution is deliberately **absent** here: it is deferred to the
/// writer so it runs once per *distinct path* **after** the dedup/debounce
/// operators have collapsed a burst (a `cargo build` opens tens of thousands of
/// files). The struct must be `Clone` for the reactive `Subject<T: Clone>` bound.
#[derive(Debug, Clone, Serialize)]
pub struct FileTouchEvent {
    /// Capture mechanism — the `source` column.
    pub source: FileEventSource,
    /// What happened to the file — the `op` column.
    pub op: FileOp,
    /// Absolute path; recorded even when the file is not (yet) indexed
    /// (`file_id` then resolves to `NULL`).
    pub abs_path: String,
    /// Acting PID (PID-native sources); `None` on the hook side.
    pub pid: Option<i32>,
    /// Immediate parent PID — advisory/forensic backstop for attribution.
    pub ppid: Option<i32>,
    /// Owning agent/session-leader PID — advisory/forensic.
    pub root_pid: Option<i32>,
    /// cgroup-v2 id (the cgroup directory inode) — the subtree-attribution key
    /// joining to `mcp_clients.cgroup_id`. Persisted as `BIGINT` via a `as i64`
    /// bit-cast (inode values fit below 2^63 in practice).
    pub cgroup_id: Option<u64>,
    /// MCP streamable-HTTP session id (PID-native sources, after resolution).
    pub mcp_session_id: Option<String>,
    /// Claude/Codex hook session UUID (the hook side).
    pub session_id: Option<Uuid>,
    /// Which agent produced it (`claude-code` | `codex` | `pi` | …) — orthogonal
    /// to `source`, which records the *mechanism*, not the agent.
    pub agent_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn file_op_vocabulary_is_pinned() {
        let got: HashSet<&str> = FileOp::ALL.iter().map(|o| o.as_str()).collect();
        let expected: HashSet<&str> = ["open", "read", "write", "edit", "close"]
            .into_iter()
            .collect();
        assert_eq!(got, expected, "FileOp vocabulary drifted from pinned set");
        assert_eq!(FileOp::ALL.len(), 5);
        assert_eq!(got.len(), 5, "duplicate as_str() in FileOp");
    }

    #[test]
    fn file_event_source_vocabulary_is_pinned() {
        let got: HashSet<&str> = FileEventSource::ALL.iter().map(|s| s.as_str()).collect();
        let expected: HashSet<&str> = ["client_hook", "ebpf", "proc_fd", "ebpf_cgroup", "preload"]
            .into_iter()
            .collect();
        assert_eq!(got, expected, "FileEventSource vocabulary drifted");
        assert_eq!(FileEventSource::ALL.len(), 5);
        assert_eq!(got.len(), 5, "duplicate as_str() in FileEventSource");
    }

    #[test]
    fn parse_roundtrips_for_all() {
        for o in FileOp::ALL {
            assert_eq!(FileOp::parse(o.as_str()), Some(*o));
        }
        assert_eq!(FileOp::parse("nonsense"), None);
        for s in FileEventSource::ALL {
            assert_eq!(FileEventSource::parse(s.as_str()), Some(*s));
        }
        assert_eq!(FileEventSource::parse("nonsense"), None);
    }

    #[test]
    fn sql_in_lists_quote_every_value() {
        let o = op_sql_in_list();
        assert!(o.contains("'edit'"), "got: {o}");
        assert_eq!(o.matches('\'').count(), FileOp::ALL.len() * 2);
        assert_eq!(o.matches(',').count(), FileOp::ALL.len() - 1);
        let s = source_sql_in_list();
        assert!(s.contains("'client_hook'"), "got: {s}");
        assert_eq!(s.matches('\'').count(), FileEventSource::ALL.len() * 2);
        assert_eq!(s.matches(',').count(), FileEventSource::ALL.len() - 1);
    }

    #[test]
    fn is_write_classifies_modifications() {
        assert!(FileOp::Write.is_write());
        assert!(FileOp::Edit.is_write());
        assert!(FileOp::Close.is_write());
        assert!(!FileOp::Read.is_write());
        assert!(!FileOp::Open.is_write());
    }
}
