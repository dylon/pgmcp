//! **Speculative prefetch**: page in pages likely to be demanded next, but only
//! with budget headroom left after demand — *never* evicting a demand page.
//!
//! ## Signals
//!
//! Prefetch candidates for an `anchor` (a resident page's address / a graph
//! node-id) are the union of three graph signals, deduped against the resident
//! set and ranked by `signal × importance`:
//!
//! 1. **Co-change coupling** — [`crate::db::queries::topics::find_coupled_files`]:
//!    files that historically change together with the anchor's file (Jaccard).
//! 2. **Memory-graph neighbors** — [`crate::db::queries::memory_search::memory_neighbors`]:
//!    the anchor's neighborhood in the unified knowledge graph (the same
//!    substrate `memory_ppr_search` runs personalized-PageRank over; here we take
//!    the bounded BFS neighborhood as the PPR/PathRAG signal without needing an
//!    anchor embedding, which the control plane does not hold at this layer).
//! 3. **(extensible)** additional [`PrefetchSource`]s can contribute candidates;
//!    the ranking/dedup/cap core is signal-agnostic.
//!
//! ## The hard rule
//!
//! Prefetch is *demand-subordinate*: it consumes only the headroom that remains
//! after demand paging, and the engine's prefetch entry point passes the
//! remaining budget as a cap so a prefetch can never trigger an eviction. This
//! is enforced structurally — [`prefetch_candidates`] returns at most the
//! candidates that fit in the *current* headroom, in rank order, and the caller
//! pages them in with a no-evict guard.
//!
//! ## Boundary & determinism
//!
//! Pure coordination/MEMORY: DB reads of pgmcp's own graph tables. Ranking is a
//! deterministic function of the signal values + importances + the working
//! set's headroom, so a replayed trace prefetches identically.

use async_trait::async_trait;
use sqlx::PgPool;
use tracing::error;

use crate::tape::data_plane::PageRef;
use crate::tape::vocab::PageKind;
use crate::tape::working_set::{PageAddr, WorkingSet};

/// A scored prefetch candidate (before dedup / budgeting).
#[derive(Debug, Clone, PartialEq)]
pub struct PrefetchCandidate {
    pub addr: PageAddr,
    pub kind: PageKind,
    pub est_tokens: i32,
    pub importance: f32,
    /// Strength of the signal that surfaced this candidate in `[0, 1]` (Jaccard,
    /// inverse BFS depth, …). The rank key is `signal × importance`.
    pub signal: f32,
}

impl PrefetchCandidate {
    /// The ranking key: stronger signal and higher importance rank first.
    pub fn rank(&self) -> f32 {
        self.signal * self.importance
    }
}

/// A source of prefetch candidates for an anchor. Implemented by the DB-backed
/// [`GraphPrefetchSource`]; tests supply an in-memory source so the ranking core
/// is exercised without a database.
#[async_trait]
pub trait PrefetchSource: Send + Sync {
    /// Candidates this source proposes for `anchor`. May overlap with other
    /// sources / the resident set; dedup is the core's job.
    async fn candidates(&self, anchor: &PageAddr) -> Vec<PrefetchCandidate>;
}

/// Deduplicate (by address, keeping the highest-ranked occurrence), drop any
/// already resident, sort by rank descending (ties by address), and greedily
/// admit in rank order while they fit in `headroom`. Returns the [`PageRef`]s to
/// page in — at most what fits, so a no-evict prefetch is structurally
/// guaranteed.
pub fn rank_and_budget(
    mut candidates: Vec<PrefetchCandidate>,
    ws: &WorkingSet,
    headroom: i32,
    cap: usize,
) -> Vec<PageRef> {
    use std::collections::HashMap;

    // Dedup by address keeping the best rank.
    let mut best: HashMap<String, PrefetchCandidate> = HashMap::with_capacity(candidates.len());
    for c in candidates.drain(..) {
        if ws.pages.contains(&c.addr) {
            continue; // never re-fetch a resident page
        }
        match best.get(&c.addr.0) {
            Some(existing) if existing.rank() >= c.rank() => {}
            _ => {
                best.insert(c.addr.0.clone(), c);
            }
        }
    }
    let mut ranked: Vec<PrefetchCandidate> = best.into_values().collect();
    ranked.sort_by(|a, b| {
        b.rank()
            .partial_cmp(&a.rank())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.addr.0.cmp(&b.addr.0))
    });

    // Greedily admit within headroom and the count cap. A candidate that does
    // not fit is skipped (a smaller, lower-ranked one may still fit) — but we
    // never exceed headroom, so prefetch cannot force an eviction.
    let mut out: Vec<PageRef> = Vec::with_capacity(cap.min(ranked.len()));
    let mut used = 0i32;
    for c in ranked {
        if out.len() >= cap {
            break;
        }
        if c.est_tokens <= 0 {
            continue;
        }
        if used.saturating_add(c.est_tokens) > headroom {
            continue;
        }
        used = used.saturating_add(c.est_tokens);
        out.push(PageRef {
            addr: c.addr,
            kind: c.kind,
            est_tokens: c.est_tokens,
            importance: c.importance,
        });
    }
    out
}

/// Compute the prefetch set for `anchor`: union every source's candidates, then
/// [`rank_and_budget`] against the working set's **current** headroom (post-
/// demand). The returned refs all fit, so paging them in cannot evict anything.
///
/// `cap` bounds the number of speculative pages (a politeness limit so a single
/// anchor cannot flood the working set).
pub async fn prefetch_candidates(
    ws: &WorkingSet,
    sources: &[&dyn PrefetchSource],
    anchor: &PageAddr,
    cap: usize,
) -> Vec<PageRef> {
    let headroom = ws.headroom().max(0);
    if headroom == 0 {
        return Vec::new(); // no room → no speculation
    }
    let mut all: Vec<PrefetchCandidate> = Vec::new();
    for src in sources {
        all.extend(src.candidates(anchor).await);
    }
    rank_and_budget(all, ws, headroom, cap)
}

// ===========================================================================
// DB-backed prefetch source (co-change + memory-graph neighbors)
// ===========================================================================

/// The production prefetch source: unions co-change coupling and memory-graph
/// neighborhood signals from pgmcp's own tables. `anchor_file` maps an anchor to
/// the project + file path used for the co-change query; `node_id` maps it to a
/// unified-graph node for the neighbor query (typically the address itself).
pub struct GraphPrefetchSource<'a> {
    pool: &'a PgPool,
    /// Project name for the co-change query.
    project: String,
    /// Minimum Jaccard coupling to consider (e.g. 0.3).
    min_coupling: f64,
    /// Minimum co-commits to consider (e.g. 3).
    min_commits: i32,
    /// Default importance assigned to a prefetch candidate (the control plane
    /// has no per-candidate importance from these graph signals, so it uses a
    /// modest constant so demand always out-ranks speculation).
    default_importance: f32,
    /// Estimated token cost to assume for a prefetch candidate when the data
    /// plane has not yet been consulted (a conservative placeholder; the real
    /// est_tokens is confirmed at fetch time).
    assumed_tokens: i32,
    /// Memory-graph BFS depth for the neighbor signal.
    neighbor_depth: i32,
    /// Max neighbor nodes to pull.
    neighbor_cap: i32,
}

impl<'a> GraphPrefetchSource<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: &'a PgPool,
        project: impl Into<String>,
        min_coupling: f64,
        min_commits: i32,
        default_importance: f32,
        assumed_tokens: i32,
        neighbor_depth: i32,
        neighbor_cap: i32,
    ) -> Self {
        Self {
            pool,
            project: project.into(),
            min_coupling,
            min_commits,
            default_importance,
            assumed_tokens,
            neighbor_depth,
            neighbor_cap,
        }
    }
}

#[async_trait]
impl PrefetchSource for GraphPrefetchSource<'_> {
    async fn candidates(&self, anchor: &PageAddr) -> Vec<PrefetchCandidate> {
        let mut out: Vec<PrefetchCandidate> = Vec::new();

        // --- Co-change coupling -------------------------------------------------
        match crate::db::queries::find_coupled_files(
            self.pool,
            &self.project,
            self.min_coupling,
            self.min_commits,
        )
        .await
        {
            Ok(pairs) => {
                for p in pairs {
                    // The partner of the anchor file in each coupled pair.
                    let partner = if p.file_a == anchor.0 {
                        Some(p.file_b)
                    } else if p.file_b == anchor.0 {
                        Some(p.file_a)
                    } else {
                        None
                    };
                    if let Some(partner) = partner {
                        out.push(PrefetchCandidate {
                            addr: PageAddr(partner),
                            kind: PageKind::FileChunk,
                            est_tokens: self.assumed_tokens,
                            importance: self.default_importance,
                            signal: p.jaccard.clamp(0.0, 1.0) as f32,
                        });
                    }
                }
            }
            Err(e) => {
                // ADR-021: a DB query that failed is an error!, not a warn!.
                error!(error = %e, project = self.project.as_str(), "prefetch co-change query failed");
            }
        }

        // --- Memory-graph neighbors (PPR/PathRAG substrate) ---------------------
        match crate::db::queries::memory_neighbors(
            self.pool,
            anchor.0.as_str(),
            self.neighbor_depth,
            None,
            self.neighbor_cap,
        )
        .await
        {
            Ok(neigh) => {
                for n in neigh.nodes {
                    if n.node_id == anchor.0 {
                        continue; // skip the seed itself
                    }
                    // Inverse BFS depth as the signal: depth-1 ⇒ 1.0, depth-2 ⇒
                    // 0.5, … (closer neighbors are stronger prefetch signals).
                    let depth = n.depth.max(1) as f32;
                    out.push(PrefetchCandidate {
                        addr: PageAddr(n.node_id),
                        kind: PageKind::MemoryObservation,
                        est_tokens: self.assumed_tokens,
                        importance: self.default_importance,
                        signal: 1.0 / depth,
                    });
                }
            }
            Err(e) => {
                error!(error = %e, anchor = anchor.0.as_str(), "prefetch memory-neighbor query failed");
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape::vocab::EvictionPolicy;
    use crate::tape::working_set::ResidentPage;

    fn ws_with(resident: &[(&str, i32)], budget: i32, used: i32) -> WorkingSet {
        let mut ws = WorkingSet::new("s", 0, budget, EvictionPolicy::ImportanceWeighted);
        for (addr, tokens) in resident {
            ws.pages.insert(ResidentPage {
                addr: PageAddr((*addr).into()),
                kind: PageKind::FileChunk,
                importance: 1.0,
                est_tokens: *tokens,
                use_count: 1,
                last_access_ord: 1,
                dirty: false,
                pinned: false,
                bytes: None,
            });
        }
        ws.resident_tokens = used;
        ws
    }

    fn cand(addr: &str, tokens: i32, importance: f32, signal: f32) -> PrefetchCandidate {
        PrefetchCandidate {
            addr: PageAddr(addr.into()),
            kind: PageKind::FileChunk,
            est_tokens: tokens,
            importance,
            signal,
        }
    }

    #[test]
    fn never_admits_beyond_headroom() {
        // budget 100, used 90 ⇒ headroom 10. Two 8-token candidates: only one
        // fits; the second must be skipped (no eviction implied).
        let ws = ws_with(&[("r", 90)], 100, 90);
        let cands = vec![cand("a", 8, 1.0, 0.9), cand("b", 8, 1.0, 0.8)];
        let chosen = rank_and_budget(cands, &ws, ws.headroom(), 10);
        assert_eq!(chosen.len(), 1, "only one 8-token page fits in 10 headroom");
        assert_eq!(chosen[0].addr.0, "a", "higher-ranked admitted first");
        let total: i32 = chosen.iter().map(|r| r.est_tokens).sum();
        assert!(total <= ws.headroom(), "prefetch stays within headroom");
    }

    #[test]
    fn drops_resident_and_dedups_keeping_best_rank() {
        let ws = ws_with(&[("resident", 10)], 1000, 10);
        let cands = vec![
            cand("resident", 5, 1.0, 1.0), // already resident → dropped
            cand("x", 5, 1.0, 0.2),        // weaker duplicate
            cand("x", 5, 1.0, 0.9),        // stronger duplicate kept
            cand("y", 5, 1.0, 0.5),
        ];
        let chosen = rank_and_budget(cands, &ws, ws.headroom(), 10);
        let addrs: Vec<&str> = chosen.iter().map(|r| r.addr.0.as_str()).collect();
        assert_eq!(
            addrs,
            ["x", "y"],
            "resident dropped; x kept once, ranked first"
        );
    }

    #[test]
    fn cap_bounds_count() {
        let ws = ws_with(&[], 10_000, 0);
        let cands = vec![
            cand("a", 1, 1.0, 0.9),
            cand("b", 1, 1.0, 0.8),
            cand("c", 1, 1.0, 0.7),
        ];
        let chosen = rank_and_budget(cands, &ws, ws.headroom(), 2);
        assert_eq!(chosen.len(), 2, "cap limits to 2 even though all fit");
    }

    #[test]
    fn zero_headroom_yields_nothing() {
        let ws = ws_with(&[("r", 100)], 100, 100);
        let cands = vec![cand("a", 1, 1.0, 1.0)];
        assert!(rank_and_budget(cands, &ws, ws.headroom(), 10).is_empty());
    }

    // In-memory source to exercise the async union path.
    struct StaticSource(Vec<PrefetchCandidate>);
    #[async_trait]
    impl PrefetchSource for StaticSource {
        async fn candidates(&self, _anchor: &PageAddr) -> Vec<PrefetchCandidate> {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn prefetch_candidates_unions_sources_and_budgets() {
        let ws = ws_with(&[("r", 80)], 100, 80); // headroom 20
        let s1 = StaticSource(vec![cand("a", 15, 1.0, 0.9)]);
        let s2 = StaticSource(vec![cand("b", 15, 1.0, 0.95), cand("a", 15, 1.0, 0.1)]);
        let sources: Vec<&dyn PrefetchSource> = vec![&s1, &s2];
        let chosen = prefetch_candidates(&ws, &sources, &PageAddr("r".into()), 10).await;
        // b (0.95) admitted first using 15 of 20; a (0.9) would need 15 more →
        // does not fit, skipped. Never exceeds headroom.
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].addr.0, "b");
    }
}
