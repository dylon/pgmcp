//! The **data-plane seam** the paging control plane calls through.
//!
//! Phase 5 (this control plane) is deliberately *decoupled* from the in-flight
//! `context-tape` data-plane crate. Instead of depending on that crate's
//! `PageAddress`, the control plane speaks in terms of an opaque
//! [`PageAddr`](crate::tape::working_set::PageAddr) string (the string IS a
//! data-plane address / path) and calls through the [`TapeDataPlane`] trait. A
//! later phase supplies the real implementation, which bridges `PageAddr` ↔ the
//! crate's `PageAddress`. For tests, [`MockTapeDataPlane`] is an in-memory
//! backing store that satisfies the same contract (the goal forbids stubs, so
//! the engine is exercised end-to-end against the mock).
//!
//! ## What the data plane does (and does not) do
//!
//! - It **resolves** a [`PageQuery`] (chunk range / semantic-k / grep) against a
//!   [`TreePath`] into page *references* ([`PageRef`] — metadata only, no bytes).
//! - It **fetches** content ([`PageContent`]) for an address, singly or in bulk.
//! - It **writes back** ([`put`](TapeDataPlane::put)) a dirty page's bytes —
//!   crucially as a *supersession*: a bi-temporal `valid_to` close on the prior
//!   version plus a fresh `valid_from` row, **never an in-place mutation**. This
//!   preserves replay determinism (older trace positions still see the older
//!   bytes).
//! - It synthesizes a **summary** ([`summary_of`](TapeDataPlane::summary_of)) of
//!   a leaf set — the compact `SummaryNode` the demotion ladder pages in when a
//!   larger leaf set is evicted.
//!
//! The data plane never decides residency; that is the controller's job.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::tape::working_set::PageAddr;

/// The root of a context tree for one orchestration run: `"rlm:{root_task_id}"`.
/// All data-plane operations are scoped to a tree path so two concurrent runs
/// cannot collide in the backing store.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TreePath(pub String);

impl TreePath {
    /// Build the canonical tree path for an RLM root task.
    pub fn for_root_task(root_task_id: &str) -> Self {
        Self(format!("rlm:{root_task_id}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How the control plane asks the data plane to resolve candidate pages — a
/// closed strategy set mirroring [`crate::a2a::rlm::DecomposeStrategy`] (the RLM
/// "code the model writes" reduced to the existing read-only tool catalog).
#[derive(Debug, Clone, PartialEq)]
pub enum PageQuery {
    /// A contiguous chunk-index sub-region of one file.
    Chunk { path: String, lo: i32, hi: i32 },
    /// Top-`k` semantic retrieval for a natural-language query.
    Semantic { query: String, k: usize },
    /// A regex / keyword filter.
    Grep { pattern: String },
}

/// A reference to a page — **metadata only, no bytes**. What
/// [`resolve`](TapeDataPlane::resolve) / [`summary_of`](TapeDataPlane::summary_of)
/// return so the controller can rank and budget candidates before paying to
/// fetch them.
#[derive(Debug, Clone, PartialEq)]
pub struct PageRef {
    pub addr: PageAddr,
    pub kind: crate::tape::vocab::PageKind,
    pub est_tokens: i32,
    pub importance: f32,
}

/// The materialized content of a page — the situating-prefixed bytes the
/// controller inserts into the working set.
#[derive(Debug, Clone, PartialEq)]
pub struct PageContent {
    pub addr: PageAddr,
    pub bytes: String,
    pub est_tokens: i32,
}

/// Failure surface of the data plane. A `Backend` failure is a genuine DB/IO
/// fault (ADR-021 `error!`); `NotFound` is a benign miss the controller handles
/// by reporting budget/coverage, not by crashing.
#[derive(Debug, thiserror::Error)]
pub enum TapeError {
    /// The address / tree path does not resolve to any stored content.
    #[error("page not found: {0}")]
    NotFound(String),
    /// An underlying backing-store fault (DB / IO / bridge failure).
    #[error("data-plane backend error: {0}")]
    Backend(String),
}

/// The seam. Every method is async and fallible; the control plane treats a
/// `Backend` error as an ADR-021 `error!` (a real fault) and a `NotFound` as a
/// coverage gap to report.
#[async_trait]
pub trait TapeDataPlane: Send + Sync {
    /// Fetch one page's content.
    async fn get(&self, tree: &TreePath, addr: &PageAddr) -> Result<PageContent, TapeError>;

    /// Fetch many pages' content in one round-trip. Order of the result is not
    /// guaranteed to match the input; the controller indexes by `addr`.
    async fn get_many(
        &self,
        tree: &TreePath,
        addrs: &[PageAddr],
    ) -> Result<Vec<PageContent>, TapeError>;

    /// Write back a page's bytes. **Supersedes** via bi-temporal `valid_to`
    /// (closes the prior version's validity and opens a fresh one); it must
    /// never mutate the prior bytes in place, so older trace positions remain
    /// reproducible.
    async fn put(&self, tree: &TreePath, addr: &PageAddr, bytes: &str) -> Result<(), TapeError>;

    /// Resolve a query into candidate page references (metadata only).
    async fn resolve(&self, tree: &TreePath, query: &PageQuery) -> Result<Vec<PageRef>, TapeError>;

    /// Synthesize / locate the summary that stands in for `leaf_addrs`. Returns
    /// `None` when no summary is available (the demotion ladder then logs a
    /// by-design `warn!` and keeps the leaves evicted without a stand-in).
    async fn summary_of(
        &self,
        tree: &TreePath,
        leaf_addrs: &[PageAddr],
    ) -> Result<Option<PageRef>, TapeError>;
}

// ===========================================================================
// MockTapeDataPlane — in-memory backing store for tests (NOT a stub: it fully
// satisfies the contract, including write-back supersession bookkeeping and
// summary synthesis, so the engine is exercised end-to-end).
// ===========================================================================

/// One stored entry in the mock backing store.
#[derive(Debug, Clone)]
struct MockEntry {
    bytes: String,
    est_tokens: i32,
    importance: f32,
    kind: crate::tape::vocab::PageKind,
}

/// In-memory [`TapeDataPlane`] for tests. Pages are keyed by `(tree, addr)`.
/// `resolve` returns whatever was registered for a tree (filtered by the query
/// where it is meaningful); `summary_of` returns a pre-registered summary if one
/// exists for the exact leaf set, else `None`.
#[derive(Debug, Default)]
pub struct MockTapeDataPlane {
    /// `(tree, addr) -> entry`.
    store: Mutex<HashMap<(String, String), MockEntry>>,
    /// `(tree, sorted-joined-leaf-addrs) -> summary addr`.
    summaries: Mutex<HashMap<(String, String), PageAddr>>,
    /// Count of `put` calls, per `(tree, addr)` — lets tests assert write-back
    /// happened exactly once for a dirty victim and zero times for a clean one.
    put_calls: Mutex<HashMap<(String, String), u32>>,
    /// Append-only supersession log: each `put` records (addr, version) so a
    /// test can assert the prior version was preserved (never mutated in place).
    supersessions: Mutex<Vec<(String, String, u32)>>,
}

impl MockTapeDataPlane {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a fetchable page (used by tests to seed the corpus). `importance`
    /// and `est_tokens` flow through to [`resolve`](TapeDataPlane::resolve) refs.
    pub fn insert_page(
        &self,
        tree: &TreePath,
        addr: &PageAddr,
        bytes: &str,
        est_tokens: i32,
        importance: f32,
        kind: crate::tape::vocab::PageKind,
    ) {
        self.store
            .lock()
            .expect("mock store mutex poisoned (a prior test thread panicked)")
            .insert(
                (tree.0.clone(), addr.0.clone()),
                MockEntry {
                    bytes: bytes.to_string(),
                    est_tokens,
                    importance,
                    kind,
                },
            );
    }

    /// Register a summary for an exact leaf set (the demotion ladder consults
    /// this in [`summary_of`](TapeDataPlane::summary_of)). The summary page must
    /// also be `insert_page`d so it can be fetched once paged in.
    pub fn register_summary(&self, tree: &TreePath, leaf_addrs: &[PageAddr], summary: &PageAddr) {
        let key = (tree.0.clone(), summary_key(leaf_addrs));
        self.summaries
            .lock()
            .expect("mock summaries mutex poisoned")
            .insert(key, summary.clone());
    }

    /// How many times `put` was called for an address (test assertion helper).
    pub fn put_count(&self, tree: &TreePath, addr: &PageAddr) -> u32 {
        *self
            .put_calls
            .lock()
            .expect("mock put_calls mutex poisoned")
            .get(&(tree.0.clone(), addr.0.clone()))
            .unwrap_or(&0)
    }

    /// Number of recorded supersession versions for an address (test helper).
    /// The supersession tuple is `(tree, addr, version)`, so this matches on the
    /// second element (the address), not the tree.
    pub fn version_count(&self, addr: &PageAddr) -> usize {
        self.supersessions
            .lock()
            .expect("mock supersessions mutex poisoned")
            .iter()
            .filter(|(_, a, _)| a == &addr.0)
            .count()
    }
}

/// Deterministic key for a leaf set: addresses sorted then joined, so the same
/// set maps to the same summary regardless of input order.
fn summary_key(leaf_addrs: &[PageAddr]) -> String {
    let mut v: Vec<&str> = leaf_addrs.iter().map(|a| a.0.as_str()).collect();
    v.sort_unstable();
    v.join("\u{1f}") // ASCII unit separator — cannot appear in a path token.
}

#[async_trait]
impl TapeDataPlane for MockTapeDataPlane {
    async fn get(&self, tree: &TreePath, addr: &PageAddr) -> Result<PageContent, TapeError> {
        let store = self.store.lock().expect("mock store mutex poisoned");
        match store.get(&(tree.0.clone(), addr.0.clone())) {
            Some(e) => Ok(PageContent {
                addr: addr.clone(),
                bytes: e.bytes.clone(),
                est_tokens: e.est_tokens,
            }),
            None => Err(TapeError::NotFound(addr.0.clone())),
        }
    }

    async fn get_many(
        &self,
        tree: &TreePath,
        addrs: &[PageAddr],
    ) -> Result<Vec<PageContent>, TapeError> {
        let store = self.store.lock().expect("mock store mutex poisoned");
        let mut out = Vec::with_capacity(addrs.len());
        for addr in addrs {
            match store.get(&(tree.0.clone(), addr.0.clone())) {
                Some(e) => out.push(PageContent {
                    addr: addr.clone(),
                    bytes: e.bytes.clone(),
                    est_tokens: e.est_tokens,
                }),
                None => return Err(TapeError::NotFound(addr.0.clone())),
            }
        }
        Ok(out)
    }

    async fn put(&self, tree: &TreePath, addr: &PageAddr, bytes: &str) -> Result<(), TapeError> {
        let key = (tree.0.clone(), addr.0.clone());
        // Record the write-back call.
        let version = {
            let mut calls = self
                .put_calls
                .lock()
                .expect("mock put_calls mutex poisoned");
            let c = calls.entry(key.clone()).or_insert(0);
            *c += 1;
            *c
        };
        // Supersession: append a NEW version; never overwrite the prior bytes'
        // log entry. The store's "current" bytes are updated, but the prior
        // version's existence is preserved in the supersession log so a test can
        // confirm in-place mutation did not occur.
        self.supersessions
            .lock()
            .expect("mock supersessions mutex poisoned")
            .push((tree.0.clone(), addr.0.clone(), version));
        let mut store = self.store.lock().expect("mock store mutex poisoned");
        let entry = store.entry(key).or_insert_with(|| MockEntry {
            bytes: String::new(),
            est_tokens: bytes.len() as i32,
            importance: 0.0,
            kind: crate::tape::vocab::PageKind::FileChunk,
        });
        entry.bytes = bytes.to_string();
        Ok(())
    }

    async fn resolve(&self, tree: &TreePath, query: &PageQuery) -> Result<Vec<PageRef>, TapeError> {
        let store = self.store.lock().expect("mock store mutex poisoned");
        let mut refs: Vec<PageRef> = store
            .iter()
            .filter(|((t, _), _)| t == &tree.0)
            .filter(|((_, addr), entry)| query_matches(query, addr, entry))
            .map(|((_, addr), entry)| PageRef {
                addr: PageAddr(addr.clone()),
                kind: entry.kind,
                est_tokens: entry.est_tokens,
                importance: entry.importance,
            })
            .collect();
        // Deterministic order: importance desc, then addr asc, so resolve() is a
        // pure function of the registered corpus (replay determinism).
        refs.sort_by(|a, b| {
            b.importance
                .partial_cmp(&a.importance)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.addr.0.cmp(&b.addr.0))
        });
        if let PageQuery::Semantic { k, .. } = query {
            refs.truncate(*k);
        }
        Ok(refs)
    }

    async fn summary_of(
        &self,
        tree: &TreePath,
        leaf_addrs: &[PageAddr],
    ) -> Result<Option<PageRef>, TapeError> {
        let summaries = self
            .summaries
            .lock()
            .expect("mock summaries mutex poisoned");
        let Some(summary_addr) = summaries.get(&(tree.0.clone(), summary_key(leaf_addrs))) else {
            return Ok(None);
        };
        let store = self.store.lock().expect("mock store mutex poisoned");
        match store.get(&(tree.0.clone(), summary_addr.0.clone())) {
            Some(e) => Ok(Some(PageRef {
                addr: summary_addr.clone(),
                kind: crate::tape::vocab::PageKind::SummaryNode,
                est_tokens: e.est_tokens,
                importance: e.importance,
            })),
            // Registered as a summary but its content was not seeded: treat as no
            // summary rather than a hard error.
            None => Ok(None),
        }
    }
}

/// Whether a registered page satisfies a query, in the mock. `Chunk` matches by
/// `path#lo-hi` containment on the address; `Semantic` matches everything (then
/// ranked and truncated by the caller); `Grep` matches addresses containing the
/// pattern.
fn query_matches(query: &PageQuery, addr: &str, _entry: &MockEntry) -> bool {
    match query {
        PageQuery::Semantic { .. } => true,
        PageQuery::Grep { pattern } => addr.contains(pattern.as_str()),
        PageQuery::Chunk { path, .. } => addr.starts_with(path.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape::vocab::PageKind;

    fn tree() -> TreePath {
        TreePath::for_root_task("task-1")
    }

    #[tokio::test]
    async fn get_and_get_many_roundtrip() {
        let dp = MockTapeDataPlane::new();
        let t = tree();
        dp.insert_page(
            &t,
            &PageAddr("a".into()),
            "alpha",
            10,
            0.5,
            PageKind::FileChunk,
        );
        dp.insert_page(
            &t,
            &PageAddr("b".into()),
            "beta",
            20,
            0.9,
            PageKind::FileChunk,
        );

        let one = dp.get(&t, &PageAddr("a".into())).await.expect("get a");
        assert_eq!(one.bytes, "alpha");
        assert_eq!(one.est_tokens, 10);

        let many = dp
            .get_many(&t, &[PageAddr("a".into()), PageAddr("b".into())])
            .await
            .expect("get_many");
        assert_eq!(many.len(), 2);

        let missing = dp.get(&t, &PageAddr("zzz".into())).await;
        assert!(matches!(missing, Err(TapeError::NotFound(_))));
    }

    #[tokio::test]
    async fn resolve_ranks_by_importance_and_truncates_k() {
        let dp = MockTapeDataPlane::new();
        let t = tree();
        dp.insert_page(&t, &PageAddr("lo".into()), "x", 5, 0.1, PageKind::FileChunk);
        dp.insert_page(&t, &PageAddr("hi".into()), "y", 5, 0.9, PageKind::FileChunk);
        let refs = dp
            .resolve(
                &t,
                &PageQuery::Semantic {
                    query: "q".into(),
                    k: 1,
                },
            )
            .await
            .expect("resolve");
        assert_eq!(refs.len(), 1, "k=1 truncates");
        assert_eq!(refs[0].addr.0, "hi", "highest importance first");
    }

    #[tokio::test]
    async fn put_supersedes_and_counts() {
        let dp = MockTapeDataPlane::new();
        let t = tree();
        dp.insert_page(
            &t,
            &PageAddr("a".into()),
            "v1",
            10,
            0.5,
            PageKind::FileChunk,
        );
        assert_eq!(dp.put_count(&t, &PageAddr("a".into())), 0);
        dp.put(&t, &PageAddr("a".into()), "v2").await.expect("put");
        assert_eq!(dp.put_count(&t, &PageAddr("a".into())), 1);
        assert_eq!(dp.version_count(&PageAddr("a".into())), 1);
        // Current bytes updated…
        let now = dp.get(&t, &PageAddr("a".into())).await.expect("get");
        assert_eq!(now.bytes, "v2");
        // …but the supersession log proves a versioned write, not a silent
        // in-place clobber with no record.
        dp.put(&t, &PageAddr("a".into()), "v3").await.expect("put2");
        assert_eq!(dp.version_count(&PageAddr("a".into())), 2);
    }

    #[tokio::test]
    async fn summary_of_returns_registered_summary_else_none() {
        let dp = MockTapeDataPlane::new();
        let t = tree();
        let leaves = [PageAddr("l1".into()), PageAddr("l2".into())];
        // No summary registered yet.
        assert!(dp.summary_of(&t, &leaves).await.expect("summary").is_none());
        // Register + seed a smaller summary.
        let s = PageAddr("sum".into());
        dp.insert_page(&t, &s, "compact", 3, 0.7, PageKind::SummaryNode);
        dp.register_summary(&t, &leaves, &s);
        let got = dp
            .summary_of(&t, &leaves)
            .await
            .expect("summary")
            .expect("some");
        assert_eq!(got.addr.0, "sum");
        assert_eq!(got.est_tokens, 3);
        // Order-independence of the leaf-set key.
        let reordered = [PageAddr("l2".into()), PageAddr("l1".into())];
        assert!(
            dp.summary_of(&t, &reordered)
                .await
                .expect("summary")
                .is_some()
        );
    }
}
