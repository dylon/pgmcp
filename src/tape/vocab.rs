//! Closed vocabularies for the Crucible **context-tape paging control plane**
//! (Phase 5), following the ADR-003 closed-set idiom: a `TEXT` column + a
//! `CHECK` built from a closed Rust enum's [`sql_in_list`](PageState::sql_in_list),
//! with a `#[cfg(test)]` golden test pinning each vocabulary — the same idiom as
//! [`crate::experiment::vocab`] and
//! [`crate::csm::session_store::SessionStatus`].
//!
//! ## What these vocabularies govern
//!
//! The paging control plane treats a model's context window as a fixed *token
//! budget* and the indexed corpus as backing store. A *page* is one resident
//! unit (a file chunk, a memory observation, or a demotion summary). The four
//! vocabularies pin:
//!
//! - [`PageState`] — the residency lifecycle of a page in `working_set_pages.state`.
//! - [`EvictionPolicy`] — which mechanical residency policy a session runs,
//!   stored in `working_set_config.policy`.
//! - [`EvictReason`] — why a page left residency (audit only; not a DB column on
//!   its own table but recorded on the page row when it is evicted).
//! - [`PageKind`] — what a resident page *is*, in `working_set_pages.page_kind`.
//!
//! ## The trust boundary
//!
//! These are *mechanical* vocabularies: the controller chooses residency from
//! the budget and the policy, never from agent judgment. There is deliberately
//! no "agent-decided" state or reason — mirroring the absence of an `Agent` arm
//! in [`crate::tracker::transition`]. Nothing here lets an agent pin/evict a
//! page by fiat; pinning is a structural property of the page, not a status an
//! agent can assert to dodge the budget.

use serde::{Deserialize, Serialize};

use crate::tracker::kind::join_quoted;

/// Residency lifecycle of a working-set page. Stored in
/// `working_set_pages.state` (default `resident`).
///
/// Transitions are mechanical and driven by the budget:
/// `Resident → Evicted` under budget pressure (or TTL / supersession);
/// `Resident → Dirty` when a write-back is owed before eviction;
/// `Resident → Pinned` when a page must never be evicted (a demand anchor the
/// controller is forbidden to drop). `Pinned` is *not* an agent-assertable
/// status — it is set structurally when a page is paged in as a hard anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageState {
    /// In the working set, clean, evictable under pressure.
    Resident,
    /// In the working set and exempt from eviction (a hard anchor).
    Pinned,
    /// In the working set but carries unflushed mutations; a write-back
    /// (`data_plane.put`) is owed before it may be evicted.
    Dirty,
    /// No longer resident — paged out (the row is retained for audit / replay
    /// determinism; the bytes live in the data plane / its summary).
    Evicted,
}

impl PageState {
    /// Canonical ordering; also the source of the DB CHECK vocabulary.
    pub const ALL: &'static [PageState] =
        &[Self::Resident, Self::Pinned, Self::Dirty, Self::Evicted];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resident => "resident",
            Self::Pinned => "pinned",
            Self::Dirty => "dirty",
            Self::Evicted => "evicted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }

    /// SQL `IN (...)` value list — the single source of truth shared with the
    /// `working_set_pages_state_check` constraint (v51 migration).
    pub fn sql_in_list() -> String {
        join_quoted(Self::ALL.iter().map(|x| x.as_str()))
    }
}

/// The mechanical eviction policy a session's controller runs. Stored in
/// `working_set_config.policy` (default `importance_weighted`).
///
/// The first four delegate to liblevenshtein's eviction wrappers
/// (`Lru`/`Lfu`/`Ttl`/`Age`) over *logical-clock* recency/frequency metadata
/// (never wall-time — see [`crate::tape::engine`]); `CostAware` and
/// `ImportanceWeighted` are pgmcp-native scorers. All are deterministic
/// functions of the replayed trace, so a resumed session reconstructs a
/// bit-identical working set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvictionPolicy {
    /// Least Recently Used — evict the page with the oldest `last_access_ord`.
    Lru,
    /// Least Frequently Used — evict the page with the smallest `use_count`.
    Lfu,
    /// Time-To-Live — evict pages whose logical age exceeds `ttl_secs` first.
    Ttl,
    /// First In First Out — evict the earliest-inserted page (insertion order).
    Fifo,
    /// Cost-aware — `(age × est_tokens) / (use_count + 1)`, evict max first.
    CostAware,
    /// Importance-weighted — `(clock − last_access_ord) / (importance ×
    /// (use_count + 1))`, evict max first. The default; keeps high-importance,
    /// frequently-used pages resident longest.
    ImportanceWeighted,
}

impl EvictionPolicy {
    /// Canonical ordering; also the source of the DB CHECK vocabulary.
    pub const ALL: &'static [EvictionPolicy] = &[
        Self::Lru,
        Self::Lfu,
        Self::Ttl,
        Self::Fifo,
        Self::CostAware,
        Self::ImportanceWeighted,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lru => "lru",
            Self::Lfu => "lfu",
            Self::Ttl => "ttl",
            Self::Fifo => "fifo",
            Self::CostAware => "cost_aware",
            Self::ImportanceWeighted => "importance_weighted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }

    /// SQL `IN (...)` value list shared with the
    /// `working_set_config_policy_check` constraint (v51 migration).
    pub fn sql_in_list() -> String {
        join_quoted(Self::ALL.iter().map(|x| x.as_str()))
    }
}

/// Why a page was evicted from residency — recorded on the page row at eviction
/// time for audit and replay diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvictReason {
    /// The token budget was exceeded; the policy selected this victim.
    BudgetPressure,
    /// The page's logical age exceeded the configured TTL.
    Ttl,
    /// An explicit, caller-requested eviction (not budget-driven).
    Explicit,
    /// The page was superseded by a newer write-back (bi-temporal `valid_to`).
    Superseded,
}

impl EvictReason {
    /// Canonical ordering; also the source of the DB CHECK vocabulary.
    pub const ALL: &'static [EvictReason] = &[
        Self::BudgetPressure,
        Self::Ttl,
        Self::Explicit,
        Self::Superseded,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::BudgetPressure => "budget_pressure",
            Self::Ttl => "ttl",
            Self::Explicit => "explicit",
            Self::Superseded => "superseded",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }

    /// SQL `IN (...)` value list (the `evict_reason` column accepts these).
    pub fn sql_in_list() -> String {
        join_quoted(Self::ALL.iter().map(|x| x.as_str()))
    }
}

/// What a resident page *is*. Stored in `working_set_pages.page_kind`.
///
/// The three kinds correspond to the three backing sources the data plane
/// resolves: indexed file chunks, memory observations, and the **summary
/// nodes** synthesized by the demotion ladder (a compact stand-in paged in when
/// a larger leaf set is evicted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageKind {
    /// A contiguous chunk of an indexed file (`file_chunks`).
    FileChunk,
    /// A memory observation / entity (`memory_*`).
    MemoryObservation,
    /// A demotion summary standing in for a set of evicted leaves.
    SummaryNode,
}

impl PageKind {
    /// Canonical ordering; also the source of the DB CHECK vocabulary.
    pub const ALL: &'static [PageKind] =
        &[Self::FileChunk, Self::MemoryObservation, Self::SummaryNode];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::FileChunk => "file_chunk",
            Self::MemoryObservation => "memory_observation",
            Self::SummaryNode => "summary_node",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }

    /// SQL `IN (...)` value list shared with the
    /// `working_set_pages_page_kind_check` constraint (v51 migration).
    pub fn sql_in_list() -> String {
        join_quoted(Self::ALL.iter().map(|x| x.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// ADR-003 golden test: each vocabulary's `as_str()` set is pinned, so a DB
    /// CHECK built from `sql_in_list()` and existing rows cannot silently drift
    /// when the enum is edited.
    #[test]
    fn vocabularies_are_pinned() {
        fn check(got: HashSet<&str>, expected: &[&str]) {
            let exp: HashSet<&str> = expected.iter().copied().collect();
            assert_eq!(got, exp, "tape vocabulary drifted from pinned set");
            assert_eq!(got.len(), expected.len(), "duplicate as_str() value");
        }
        check(
            PageState::ALL.iter().map(|x| x.as_str()).collect(),
            &["resident", "pinned", "dirty", "evicted"],
        );
        check(
            EvictionPolicy::ALL.iter().map(|x| x.as_str()).collect(),
            &[
                "lru",
                "lfu",
                "ttl",
                "fifo",
                "cost_aware",
                "importance_weighted",
            ],
        );
        check(
            EvictReason::ALL.iter().map(|x| x.as_str()).collect(),
            &["budget_pressure", "ttl", "explicit", "superseded"],
        );
        check(
            PageKind::ALL.iter().map(|x| x.as_str()).collect(),
            &["file_chunk", "memory_observation", "summary_node"],
        );
    }

    #[test]
    fn parse_roundtrips_for_all() {
        for x in PageState::ALL {
            assert_eq!(PageState::parse(x.as_str()), Some(*x));
        }
        for x in EvictionPolicy::ALL {
            assert_eq!(EvictionPolicy::parse(x.as_str()), Some(*x));
        }
        for x in EvictReason::ALL {
            assert_eq!(EvictReason::parse(x.as_str()), Some(*x));
        }
        for x in PageKind::ALL {
            assert_eq!(PageKind::parse(x.as_str()), Some(*x));
        }
        assert_eq!(PageState::parse("nonsense"), None);
        assert_eq!(EvictionPolicy::parse("nonsense"), None);
        assert_eq!(EvictReason::parse("nonsense"), None);
        assert_eq!(PageKind::parse("nonsense"), None);
    }

    #[test]
    fn sql_in_list_quotes_every_value() {
        for (list, n, first) in [
            (PageState::sql_in_list(), PageState::ALL.len(), "'resident'"),
            (
                EvictionPolicy::sql_in_list(),
                EvictionPolicy::ALL.len(),
                "'lru'",
            ),
            (
                EvictReason::sql_in_list(),
                EvictReason::ALL.len(),
                "'budget_pressure'",
            ),
            (PageKind::sql_in_list(), PageKind::ALL.len(), "'file_chunk'"),
        ] {
            assert!(list.starts_with(first), "got: {list}");
            assert_eq!(list.matches('\'').count(), n * 2, "quote count: {list}");
            assert_eq!(list.matches(',').count(), n - 1, "comma count: {list}");
        }
    }
}
