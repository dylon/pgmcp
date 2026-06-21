//! The in-memory **working set**: the multiset of pages resident for one
//! orchestration session at one trace position, plus the residency bookkeeping
//! the [`engine`](crate::tape::engine) mutates.
//!
//! ## Insertion-ordered map without a new dependency
//!
//! The control-plane design calls for an insertion-order-preserving map (FIFO
//! eviction needs insertion order; address lookup needs O(1)). The natural fit
//! is `indexmap::IndexMap`, but `indexmap` is only a *transitive* dependency of
//! pgmcp, not a declared one, and this phase must add **no new external crate
//! dependency**. So [`OrderedPages`] provides exactly the two operations the
//! engine needs — O(1) `get`/`insert`/`remove` by [`PageAddr`] *and* a stable
//! insertion-order iterator — backed by `std`'s `HashMap` plus a `Vec` order
//! index. (Removal is swap-free: it marks a tombstone in the order index and
//! compacts lazily, so insertion order among live entries is preserved.)
//!
//! ## Logical clock, not wall-time
//!
//! Every [`ResidentPage::last_access_ord`] is a snapshot of the session's
//! monotonic logical clock ([`WorkingSet::clock`]), never wall-time. This is the
//! determinism anchor: replaying the same page-in / evict sequence advances the
//! clock identically, so the reconstructed working set is bit-identical.

use std::collections::HashMap;

use crate::tape::vocab::EvictionPolicy;

/// An opaque data-plane address. The string *is* the address/path; the control
/// plane never interprets its structure (a later phase bridges it to the
/// data-plane crate's `PageAddress`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PageAddr(pub String);

impl PageAddr {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for PageAddr {
    fn from(s: &str) -> Self {
        PageAddr(s.to_string())
    }
}

impl From<String> for PageAddr {
    fn from(s: String) -> Self {
        PageAddr(s)
    }
}

/// One resident page and its mechanical residency metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct ResidentPage {
    pub addr: PageAddr,
    pub kind: crate::tape::vocab::PageKind,
    /// Caller-supplied salience in `[0, ∞)`; higher = keep longer. Drives the
    /// importance-weighted policy and breaks ties in others.
    pub importance: f32,
    /// Estimated token cost of this page's situated content. The budget is in
    /// these units; `WorkingSet::resident_tokens` is their running sum.
    pub est_tokens: i32,
    /// How many times this page has been demanded (frequency signal for LFU and
    /// the importance-weighted scorer).
    pub use_count: u32,
    /// Logical-clock value at last access (recency signal). **Never wall-time.**
    pub last_access_ord: u64,
    /// A write-back (`data_plane.put`) is owed before eviction.
    pub dirty: bool,
    /// Exempt from eviction (a hard demand anchor).
    pub pinned: bool,
    /// The situated content carried on the resident page for write-back /
    /// durable persistence. `None` for corpus / observation / summary pages —
    /// they are re-fetchable from the read-only corpus, so the control plane
    /// keeps only their token/importance metadata. `Some(bytes)` **only** for a
    /// `Scratch`-kind page (accumulator / REPL output): a scratch page has no
    /// corpus source, so its bytes must travel on the resident page to be
    /// written back on eviction and persisted to `working_set_pages.content` so
    /// a paused session can rehydrate them byte-identically on resume.
    ///
    /// Does **not** participate in token accounting (the budget sums
    /// [`est_tokens`](Self::est_tokens), not `bytes.len()`), so carrying it never
    /// perturbs the resident-token invariant.
    pub bytes: Option<String>,
}

impl ResidentPage {
    /// Logical age relative to `clock` (saturating; a page touched "in the
    /// future" relative to a stale clock reads as age 0). Used by recency-based
    /// scorers and the logical TTL.
    pub fn logical_age(&self, clock: u64) -> u64 {
        clock.saturating_sub(self.last_access_ord)
    }

    /// Importance-weighted eviction score: `(clock − last_access_ord) /
    /// (importance.max(ε) × (use_count + 1))`. Higher = better eviction
    /// candidate. `ε = 1e-3` keeps a zero-importance page from dividing by zero
    /// while still ranking far below any positively-weighted page.
    pub fn importance_weighted_score(&self, clock: u64) -> f64 {
        let age = self.logical_age(clock) as f64;
        let weight = (self.importance.max(1e-3) as f64) * ((self.use_count as f64) + 1.0);
        age / weight
    }

    /// Cost-aware eviction score: `(age × est_tokens) / (use_count + 1)`. Higher
    /// = better candidate (old, large, rarely-used pages go first).
    pub fn cost_aware_score(&self, clock: u64) -> f64 {
        let age = self.logical_age(clock) as f64;
        let tokens = (self.est_tokens.max(0) as f64) + 1.0;
        (age * tokens) / ((self.use_count as f64) + 1.0)
    }
}

/// Insertion-ordered page map (the `indexmap`-free substitute described in the
/// module docs). Live entries iterate in insertion order; lookup/insert/remove
/// by address are O(1) amortized.
#[derive(Debug, Clone, Default)]
pub struct OrderedPages {
    map: HashMap<PageAddr, ResidentPage>,
    /// Insertion order of *currently or formerly* present addresses. Entries
    /// removed from `map` become tombstones here until [`compact`](Self::compact)
    /// or the next iteration prunes them.
    order: Vec<PageAddr>,
    /// Tombstone count in `order`; compaction triggers when it grows large
    /// relative to live entries (amortized O(1) removal).
    tombstones: usize,
}

impl OrderedPages {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(n: usize) -> Self {
        Self {
            map: HashMap::with_capacity(n),
            order: Vec::with_capacity(n),
            tombstones: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn contains(&self, addr: &PageAddr) -> bool {
        self.map.contains_key(addr)
    }

    pub fn get(&self, addr: &PageAddr) -> Option<&ResidentPage> {
        self.map.get(addr)
    }

    pub fn get_mut(&mut self, addr: &PageAddr) -> Option<&mut ResidentPage> {
        self.map.get_mut(addr)
    }

    /// Insert or overwrite. A brand-new address is appended to the order index;
    /// overwriting an existing address keeps its original insertion position
    /// (FIFO age is preserved across re-insert, matching `IndexMap::insert`).
    pub fn insert(&mut self, page: ResidentPage) {
        let addr = page.addr.clone();
        let is_new = !self.map.contains_key(&addr);
        self.map.insert(addr.clone(), page);
        if is_new {
            self.order.push(addr);
        }
    }

    /// Remove by address, returning the page if present. Leaves a tombstone in
    /// the order index (compacted lazily).
    pub fn remove(&mut self, addr: &PageAddr) -> Option<ResidentPage> {
        let removed = self.map.remove(addr);
        if removed.is_some() {
            self.tombstones += 1;
            self.maybe_compact();
        }
        removed
    }

    /// Iterate live pages in insertion order.
    pub fn iter_in_order(&self) -> impl Iterator<Item = &ResidentPage> {
        self.order.iter().filter_map(move |a| self.map.get(a))
    }

    /// Iterate live pages in arbitrary (hash) order — cheaper when order is
    /// irrelevant (e.g. summing tokens).
    pub fn values(&self) -> impl Iterator<Item = &ResidentPage> {
        self.map.values()
    }

    /// The first live page in insertion order (the FIFO victim), if any.
    pub fn oldest_in_order(&self) -> Option<&ResidentPage> {
        self.iter_in_order().next()
    }

    fn maybe_compact(&mut self) {
        // Compact when tombstones dominate; keeps `order` from growing unbounded
        // across many evictions while staying amortized O(1) per removal.
        if self.tombstones > self.map.len() + 8 {
            self.compact();
        }
    }

    fn compact(&mut self) {
        let live: Vec<PageAddr> = self
            .order
            .iter()
            .filter(|a| self.map.contains_key(*a))
            .cloned()
            .collect();
        self.order = live;
        self.tombstones = 0;
    }
}

/// The working set for one (session, cursor): its budget, policy, logical clock,
/// running token sum, and resident pages.
#[derive(Debug, Clone)]
pub struct WorkingSet {
    pub session_key: String,
    pub state_cursor: i32,
    /// Token budget the resident set must never exceed.
    pub budget_tokens: i32,
    /// Running Σ of `est_tokens` over resident pages (invariant maintained by
    /// the engine; property-tested).
    pub resident_tokens: i32,
    pub policy: EvictionPolicy,
    /// Monotonic logical clock; every access stamps `last_access_ord` from it.
    pub clock: u64,
    /// Logical TTL in **clock ticks** (never seconds): a resident, non-pinned
    /// page whose [`logical_age`](ResidentPage::logical_age) exceeds this is
    /// eligible for [`EvictReason::Ttl`](crate::tape::vocab::EvictReason::Ttl)
    /// eviction before budget-pressure admission. `None` (or `0`) disables
    /// logical-TTL eviction entirely. Seeded from `[tape] ttl_secs`, which the
    /// control plane interprets as logical ticks (see
    /// [`from_config_defaults`](Self::from_config_defaults)).
    pub ttl: Option<u64>,
    pub pages: OrderedPages,
}

impl WorkingSet {
    /// A fresh, empty working set with no logical TTL.
    pub fn new(
        session_key: impl Into<String>,
        state_cursor: i32,
        budget_tokens: i32,
        policy: EvictionPolicy,
    ) -> Self {
        Self {
            session_key: session_key.into(),
            state_cursor,
            budget_tokens,
            resident_tokens: 0,
            policy,
            clock: 0,
            ttl: None,
            pages: OrderedPages::new(),
        }
    }

    /// A fresh, empty working set seeded from the `[tape]` config defaults: the
    /// budget, the eviction policy (parsed from `cfg.policy`, falling back to
    /// [`EvictionPolicy::ImportanceWeighted`] on an unrecognized label), and the
    /// logical TTL.
    ///
    /// `cfg.ttl_secs` is interpreted as **logical ticks** (the control plane's
    /// TTL is measured on the monotonic logical clock, never wall-time — see the
    /// [`engine`](crate::tape::engine) module docs); a non-positive value yields
    /// `ttl = None` (TTL eviction disabled). This is the production-side
    /// constructor a caller uses before its first
    /// [`page_in`](crate::tape::engine::PagingEngine::page_in) when no persisted
    /// `working_set_config` exists yet.
    pub fn from_config_defaults(
        session_key: impl Into<String>,
        state_cursor: i32,
        cfg: &crate::config::TapeConfig,
    ) -> Self {
        let policy =
            EvictionPolicy::parse(&cfg.policy).unwrap_or(EvictionPolicy::ImportanceWeighted);
        let ttl = if cfg.ttl_secs > 0 {
            Some(cfg.ttl_secs as u64)
        } else {
            None
        };
        Self {
            session_key: session_key.into(),
            state_cursor,
            budget_tokens: cfg.budget_tokens,
            resident_tokens: 0,
            policy,
            clock: 0,
            ttl,
            pages: OrderedPages::new(),
        }
    }

    /// Advance the logical clock and return the new value. The single source of
    /// `last_access_ord` stamps — called on every page-in and on demand-hit.
    pub fn tick(&mut self) -> u64 {
        self.clock = self.clock.saturating_add(1);
        self.clock
    }

    /// Tokens that would remain free after the current resident set.
    pub fn headroom(&self) -> i32 {
        self.budget_tokens - self.resident_tokens
    }

    /// Whether `extra` more tokens fit within budget.
    pub fn fits(&self, extra: i32) -> bool {
        self.resident_tokens.saturating_add(extra) <= self.budget_tokens
    }

    /// Recompute `resident_tokens` from the resident pages — the invariant the
    /// engine maintains incrementally; used in tests and as a repair.
    pub fn recompute_resident_tokens(&self) -> i32 {
        self.pages.values().map(|p| p.est_tokens).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape::vocab::PageKind;

    fn page(addr: &str, tokens: i32, importance: f32, ord: u64) -> ResidentPage {
        ResidentPage {
            addr: PageAddr(addr.into()),
            kind: PageKind::FileChunk,
            importance,
            est_tokens: tokens,
            use_count: 0,
            last_access_ord: ord,
            dirty: false,
            pinned: false,
            bytes: None,
        }
    }

    #[test]
    fn ordered_pages_preserve_insertion_order_and_lookup() {
        let mut op = OrderedPages::new();
        op.insert(page("a", 1, 0.0, 0));
        op.insert(page("b", 1, 0.0, 0));
        op.insert(page("c", 1, 0.0, 0));
        let order: Vec<&str> = op.iter_in_order().map(|p| p.addr.0.as_str()).collect();
        assert_eq!(order, ["a", "b", "c"]);
        assert!(op.get(&PageAddr("b".into())).is_some());
        // Remove the middle; order of survivors preserved.
        op.remove(&PageAddr("b".into()));
        let order: Vec<&str> = op.iter_in_order().map(|p| p.addr.0.as_str()).collect();
        assert_eq!(order, ["a", "c"]);
        assert_eq!(op.oldest_in_order().map(|p| p.addr.0.as_str()), Some("a"));
    }

    #[test]
    fn reinsert_keeps_original_position() {
        let mut op = OrderedPages::new();
        op.insert(page("a", 1, 0.0, 0));
        op.insert(page("b", 1, 0.0, 0));
        // Overwrite "a" with a higher token count — position must remain first.
        op.insert(page("a", 99, 0.0, 5));
        let order: Vec<&str> = op.iter_in_order().map(|p| p.addr.0.as_str()).collect();
        assert_eq!(order, ["a", "b"]);
        assert_eq!(
            op.get(&PageAddr("a".into())).expect("present").est_tokens,
            99
        );
    }

    #[test]
    fn compaction_after_many_removals_keeps_order() {
        let mut op = OrderedPages::with_capacity(64);
        for i in 0..50 {
            op.insert(page(&format!("p{i}"), 1, 0.0, 0));
        }
        // Remove all even-indexed pages → forces at least one compaction.
        for i in (0..50).step_by(2) {
            op.remove(&PageAddr(format!("p{i}")));
        }
        let survivors: Vec<String> = op.iter_in_order().map(|p| p.addr.0.clone()).collect();
        let expected: Vec<String> = (1..50).step_by(2).map(|i| format!("p{i}")).collect();
        assert_eq!(
            survivors, expected,
            "insertion order preserved post-compaction"
        );
        assert_eq!(op.len(), 25);
    }

    #[test]
    fn importance_weighted_score_prefers_low_importance_stale() {
        // A stale, low-importance page must score ABOVE a stale high-importance
        // page (gets evicted first) — the discriminating property.
        let clock = 100;
        let lo = page("lo", 10, 0.1, 0); // age 100, imp 0.1
        let hi = page("hi", 10, 10.0, 0); // age 100, imp 10
        assert!(
            lo.importance_weighted_score(clock) > hi.importance_weighted_score(clock),
            "low-importance page is the better eviction candidate"
        );
    }

    #[test]
    fn logical_age_is_saturating() {
        let p = page("a", 1, 1.0, 50);
        assert_eq!(p.logical_age(70), 20);
        assert_eq!(
            p.logical_age(10),
            0,
            "future access reads as age 0, not underflow"
        );
    }

    #[test]
    fn fits_and_headroom_track_budget() {
        let mut ws = WorkingSet::new("s", 0, 100, EvictionPolicy::ImportanceWeighted);
        ws.resident_tokens = 80;
        assert_eq!(ws.headroom(), 20);
        assert!(ws.fits(20));
        assert!(!ws.fits(21));
        assert_eq!(ws.tick(), 1);
        assert_eq!(ws.tick(), 2);
    }

    #[test]
    fn from_config_defaults_seeds_budget_policy_and_logical_ttl() {
        // A recognized policy + positive ttl_secs (interpreted as logical ticks).
        // `..default()` for any unrelated fields so adding a TapeConfig field
        // never breaks this focused test.
        let cfg = crate::config::TapeConfig {
            budget_tokens: 4242,
            policy: "lru".to_string(),
            ttl_secs: 7,
            ..crate::config::TapeConfig::default()
        };
        let ws = WorkingSet::from_config_defaults("rlm:abc", 0, &cfg);
        assert_eq!(ws.budget_tokens, 4242);
        assert_eq!(ws.policy, EvictionPolicy::Lru);
        assert_eq!(ws.ttl, Some(7), "ttl_secs is read as logical ticks");
        assert_eq!(ws.clock, 0);
        assert!(ws.pages.is_empty());

        // ttl_secs = 0 ⇒ TTL disabled; an unrecognized policy falls back.
        let cfg_off = crate::config::TapeConfig {
            budget_tokens: 10,
            policy: "nonsense-policy".to_string(),
            ttl_secs: 0,
            ..crate::config::TapeConfig::default()
        };
        let ws_off = WorkingSet::from_config_defaults("s", 1, &cfg_off);
        assert_eq!(ws_off.ttl, None, "ttl_secs=0 disables logical TTL");
        assert_eq!(
            ws_off.policy,
            EvictionPolicy::ImportanceWeighted,
            "unrecognized policy falls back to importance_weighted"
        );
    }

    #[test]
    fn carried_bytes_do_not_affect_token_sum() {
        // A scratch page that carries bytes must contribute its est_tokens to the
        // running sum, NOT its byte length — the bytes are payload, not budget.
        let mut ws = WorkingSet::new("s", 0, 100, EvictionPolicy::ImportanceWeighted);
        let mut scratch = page("scratch/x", 3, 1.0, 0);
        scratch.bytes = Some("a much longer string than three tokens worth".to_string());
        ws.pages.insert(scratch);
        ws.resident_tokens = ws.recompute_resident_tokens();
        assert_eq!(
            ws.recompute_resident_tokens(),
            3,
            "token sum counts est_tokens, never bytes.len()"
        );
    }
}
