//! The **paging engine**: the mechanical heart of the control plane.
//!
//! [`PagingEngine::page_in`] brings the pages a [`PageQuery`] resolves to into
//! the [`WorkingSet`], evicting under budget pressure via the session's
//! [`EvictionPolicy`]. Every residency decision is a deterministic function of
//! the budget, the policy, and the monotonic **logical clock** — never agent
//! judgment, never wall-time.
//!
//! ## Page-in algorithm
//!
//! 1. `data_plane.resolve(query)` → candidate [`PageRef`]s.
//! 2. Process non-resident candidates in **importance order** (high first). For
//!    each candidate:
//!    - if it already exceeds budget on its own, skip it and record it in
//!      `budget_exhausted` (it can never fit);
//!    - if it does not fit in current headroom, run [`evict_to_fit`] to make
//!      room; if even that cannot free enough (only pinned pages remain), stop
//!      and report the remaining candidates as `budget_exhausted`;
//!    - otherwise admit it.
//! 3. `data_plane.get_many` the admitted addresses, situate each via
//!    [`crate::indexer::contextualize::build_context_prefix`], insert into the
//!    working set, add to `resident_tokens`, and stamp `last_access_ord = clock`
//!    (ticking the clock per admission).
//! 4. Persist the mutated working set.
//!
//! A demand-hit on an already-resident page bumps its `use_count` and restamps
//! its `last_access_ord` (so recency/frequency policies see the access) without
//! re-fetching.
//!
//! ## Eviction & the demotion ladder
//!
//! [`evict_to_fit`] repeatedly asks the policy engine for the worst victim among
//! the *unpinned* resident pages and removes it until enough room exists (or
//! only pinned pages remain). For each victim:
//!
//! - if it is **dirty**, `data_plane.put` writes it back (a supersession — see
//!   [`crate::tape::data_plane`]) — exactly once;
//! - then the **demotion ladder** asks `data_plane.summary_of([victim])` for a
//!   compact `SummaryNode`; if one exists, is smaller than the victim, and is
//!   not already resident, it is paged in to stand in for the evicted leaf;
//! - finally the page is removed from the working set and `store::evict_page`
//!   records the [`EvictReason`].
//!
//! Pinned pages are never evicted.
//!
//! ## Logging (ADR-021)
//!
//! A caught DB/IO failure or a failed `data_plane.get`/`put` logs `error!`. A
//! *by-design* budget-pressure eviction or a no-summary demotion logs `warn!`
//! with deliberately non-trigger wording ("evicting…", "no summary available")
//! so the `no_swallowed_error_warn` guard does not flag it.

use sqlx::PgPool;
use tracing::{error, warn};

use libdictenstein::MappedDictionary;
use libdictenstein::dynamic_dawg::DynamicDawg;
use liblevenshtein::cache::eviction::Lfu;

use crate::indexer::contextualize::{ChunkContext, build_context_prefix};
use crate::tape::data_plane::{PageQuery, PageRef, TapeDataPlane, TapeError, TreePath};
use crate::tape::store;
use crate::tape::vocab::{EvictReason, EvictionPolicy, PageKind};
use crate::tape::working_set::{PageAddr, ResidentPage, WorkingSet};

/// Outcome of a [`PagingEngine::page_in`]: which addresses were newly admitted,
/// which were already resident (demand-hits), which were evicted to make room,
/// and which candidates could not fit within budget.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PageInOutcome {
    /// Addresses newly brought into the working set this call.
    pub admitted: Vec<PageAddr>,
    /// Candidate addresses already resident (use-count bumped, restamped).
    pub already_resident: Vec<PageAddr>,
    /// Addresses evicted to make room for the admissions.
    pub evicted: Vec<PageAddr>,
    /// Summary addresses paged in by the demotion ladder.
    pub demoted_in: Vec<PageAddr>,
    /// Candidate addresses that did not fit within budget (reported, not fetched).
    pub budget_exhausted: Vec<PageAddr>,
}

/// A failure of the paging engine. `DataPlane` wraps a seam fault; `Db` wraps a
/// persistence fault. Both are ADR-021 `error!`-grade at the call site.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("data-plane error during paging: {0}")]
    DataPlane(#[from] TapeError),
    #[error("persistence error during paging: {0}")]
    Db(#[from] sqlx::Error),
}

// ===========================================================================
// Eviction policy engine
// ===========================================================================

/// The pluggable eviction-selection strategy. Given the resident, **unpinned**
/// pages and the current logical clock, return the address of the single worst
/// victim (the one to evict next), or `None` if there is no eligible victim.
///
/// All selections are deterministic functions of the pages' *logical* metadata
/// (`last_access_ord`, `use_count`, `est_tokens`, `importance`) — never
/// wall-time — so the choice replays identically.
// `Send + Sync`: the engine is now wired into the live MCP handler path
// (`tape_put` → `admit_scratch` → `page_in`/`evict_to_fit`), whose futures MUST be
// `Send` for axum. A `Box<dyn EvictionPolicyEngine>` is held across `.await` points
// inside the engine, so the trait object must be `Send + Sync`. Every policy is a
// stateless unit struct (`LruPolicy`, `LfuPolicy`, …), so this bound is free.
pub trait EvictionPolicyEngine: Send + Sync {
    /// Select the next victim among `candidates` (already filtered to unpinned).
    fn select_victim(&self, candidates: &[&ResidentPage], clock: u64) -> Option<PageAddr>;
}

/// Build the policy engine for a session's [`EvictionPolicy`].
pub fn policy_engine(policy: EvictionPolicy) -> Box<dyn EvictionPolicyEngine> {
    match policy {
        EvictionPolicy::Lru => Box::new(LruPolicy),
        EvictionPolicy::Lfu => Box::new(LfuPolicy),
        EvictionPolicy::Ttl => Box::new(TtlPolicy),
        EvictionPolicy::Fifo => Box::new(FifoPolicy),
        EvictionPolicy::CostAware => Box::new(CostAwarePolicy),
        EvictionPolicy::ImportanceWeighted => Box::new(ImportanceWeightedPolicy),
    }
}

/// Helper: build a `DynamicDawg<u32>` over the candidate addresses (values are a
/// stable enumeration index) so a liblevenshtein eviction wrapper can be primed
/// over it. The DAWG is keyed by the page address string.
fn candidate_dawg(candidates: &[&ResidentPage]) -> DynamicDawg<u32> {
    let entries: Vec<(String, u32)> = candidates
        .iter()
        .enumerate()
        .map(|(i, p)| (p.addr.0.clone(), i as u32))
        .collect();
    DynamicDawg::from_terms_with_values(entries)
}

/// LRU — evict the page with the oldest `last_access_ord`, a pure deterministic
/// function of the **logical** clock (never wall-time). Ties on the minimum
/// `last_access_ord` resolve to the lexically-smallest address.
///
/// (Earlier revisions routed this through liblevenshtein's `Lru` wrapper, but the
/// wrapper records recency from a wall-clock `Instant` — host-timer-dependent and
/// thus non-deterministic — so its pick was always discarded in favor of this
/// canonical logical-clock selection. The throwaway DAWG + wrapper were dead
/// work and have been removed; only the canonical selection remains.)
struct LruPolicy;
impl EvictionPolicyEngine for LruPolicy {
    fn select_victim(&self, candidates: &[&ResidentPage], _clock: u64) -> Option<PageAddr> {
        // The true LRU is the minimum `last_access_ord`; resolve ties by address.
        let min_ord = candidates.iter().map(|p| p.last_access_ord).min()?;
        candidates
            .iter()
            .filter(|p| p.last_access_ord == min_ord)
            .min_by(|a, b| a.addr.0.cmp(&b.addr.0))
            .map(|p| p.addr.clone())
    }
}

/// LFU — delegates to liblevenshtein's [`Lfu`] wrapper. We prime each candidate
/// `use_count` times (capped) so the wrapper's internal access count reproduces
/// the logical `use_count`, then `find_lfu` returns the least-frequently-used.
/// Ties broken by oldest `last_access_ord` then address.
struct LfuPolicy;
impl EvictionPolicyEngine for LfuPolicy {
    fn select_victim(&self, candidates: &[&ResidentPage], _clock: u64) -> Option<PageAddr> {
        if candidates.is_empty() {
            return None;
        }
        let dict = candidate_dawg(candidates);
        let lfu = Lfu::new(dict);
        // The wrapper sets count=1 on first access then increments; replay
        // `use_count` accesses (min 1) so its count == use_count.max(1).
        for p in candidates {
            let hits = p.use_count.max(1);
            for _ in 0..hits {
                let _ = lfu.get_value(p.addr.0.as_str());
            }
        }
        let terms: Vec<&str> = candidates.iter().map(|p| p.addr.0.as_str()).collect();
        // `find_lfu` resolves the min count; resolve ties deterministically by
        // preferring the oldest `last_access_ord` then the lexically-first addr.
        let min_addr = lfu.find_lfu(&terms)?;
        let min_count = candidates
            .iter()
            .find(|p| p.addr.0 == min_addr)
            .map(|p| p.use_count.max(1))?;
        candidates
            .iter()
            .filter(|p| p.use_count.max(1) == min_count)
            .min_by(|a, b| {
                a.last_access_ord
                    .cmp(&b.last_access_ord)
                    .then_with(|| a.addr.0.cmp(&b.addr.0))
            })
            .map(|p| p.addr.clone())
    }
}

/// TTL — evict the stalest page first: the maximum logical age
/// (`clock − last_access_ord`), measured on the **logical** clock (never
/// wall-time). Ties on equal logical age resolve to the lexically-smallest
/// address. Note this is the policy's *victim-ordering* within
/// [`evict_to_fit`](PagingEngine::evict_to_fit); the actual TTL *expiry*
/// (evicting pages whose age exceeds `ws.ttl` regardless of budget pressure) is
/// driven by [`PagingEngine::evict_expired`].
///
/// (Earlier revisions routed this through liblevenshtein's `Age` wrapper, whose
/// `is_expired` / `inserted_at` are wall-clock — incompatible with the
/// determinism constraint — so its pick was discarded in favor of this canonical
/// logical-age selection. The throwaway DAWG + wrapper were dead work and have
/// been removed.)
struct TtlPolicy;
impl EvictionPolicyEngine for TtlPolicy {
    fn select_victim(&self, candidates: &[&ResidentPage], clock: u64) -> Option<PageAddr> {
        let max_age = candidates.iter().map(|p| p.logical_age(clock)).max()?;
        candidates
            .iter()
            .filter(|p| p.logical_age(clock) == max_age)
            .min_by(|a, b| a.addr.0.cmp(&b.addr.0))
            .map(|p| p.addr.clone())
    }
}

/// FIFO — evict the FIFO front: the FIRST candidate in the engine-supplied
/// insertion order (the engine gathers candidates via
/// [`OrderedPages::iter_in_order`](crate::tape::working_set::OrderedPages::iter_in_order),
/// so position 0 is the earliest-inserted live page). This is the one policy that
/// is intentionally *not* a function of `last_access_ord` — re-access never
/// changes FIFO order.
///
/// (Earlier revisions routed this through liblevenshtein's `Age` wrapper, but the
/// FIFO front is fully determined by the engine's insertion-ordered candidate
/// slice, so the wrapper's wall-clock pick was discarded. The throwaway DAWG +
/// wrapper were dead work and have been removed.)
struct FifoPolicy;
impl EvictionPolicyEngine for FifoPolicy {
    fn select_victim(&self, candidates: &[&ResidentPage], _clock: u64) -> Option<PageAddr> {
        candidates.first().map(|p| p.addr.clone())
    }
}

/// Cost-aware — pgmcp-native: evict the page with the greatest
/// `(age × est_tokens) / (use_count + 1)`.
struct CostAwarePolicy;
impl EvictionPolicyEngine for CostAwarePolicy {
    fn select_victim(&self, candidates: &[&ResidentPage], clock: u64) -> Option<PageAddr> {
        candidates
            .iter()
            .max_by(|a, b| {
                a.cost_aware_score(clock)
                    .partial_cmp(&b.cost_aware_score(clock))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.addr.0.cmp(&b.addr.0))
            })
            .map(|p| p.addr.clone())
    }
}

/// Importance-weighted — pgmcp-native (the default policy): evict the page with
/// the greatest `(clock − last_access_ord) / (importance.max(ε) × (use_count +
/// 1))`. Keeps high-importance, frequently-used pages resident longest.
struct ImportanceWeightedPolicy;
impl EvictionPolicyEngine for ImportanceWeightedPolicy {
    fn select_victim(&self, candidates: &[&ResidentPage], clock: u64) -> Option<PageAddr> {
        candidates
            .iter()
            .max_by(|a, b| {
                a.importance_weighted_score(clock)
                    .partial_cmp(&b.importance_weighted_score(clock))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.addr.0.cmp(&b.addr.0))
            })
            .map(|p| p.addr.clone())
    }
}

// ===========================================================================
// PagingEngine
// ===========================================================================

/// The control-plane engine. Holds the DB pool and the data-plane seam; all
/// residency mutations flow through its methods.
pub struct PagingEngine<'a, D: TapeDataPlane> {
    pool: &'a PgPool,
    data_plane: &'a D,
}

impl<'a, D: TapeDataPlane> PagingEngine<'a, D> {
    pub fn new(pool: &'a PgPool, data_plane: &'a D) -> Self {
        Self { pool, data_plane }
    }

    /// Advance the **durable** logical clock by one tick and return the new
    /// value, mirroring it onto `ws.clock` so subsequent in-memory comparisons
    /// (logical age, recency) see the same value the row carries.
    ///
    /// This is the single clock authority for every *persisted* residency path
    /// (demand-hit, admit, demote-in, scratch admit). It uses the atomic relative
    /// increment [`store::bump_clock`] (`logical_clock = logical_clock + 1`
    /// RETURNING the new value) rather than the in-memory [`WorkingSet::tick`], so
    /// two writers on the same `(session_key, cursor)` never lose a tick — the
    /// determinism hazard that an absolute `save_config` overwrite of
    /// `logical_clock` would reintroduce (see [`store::save_config`]). The pure
    /// DB-free proptest model keeps using `ws.tick()`; only production paths route
    /// through here.
    async fn advance_clock(&self, ws: &mut WorkingSet) -> Result<u64, EngineError> {
        let new_clock = store::bump_clock(self.pool, &ws.session_key, 1).await?;
        let new_clock = new_clock.max(0) as u64;
        ws.clock = ws.clock.max(new_clock);
        Ok(new_clock)
    }

    /// Bring the pages a `query` resolves to into `ws`, evicting under budget
    /// pressure. See the module docs for the full algorithm. Persists the
    /// mutated working set before returning.
    pub async fn page_in(
        &self,
        ws: &mut WorkingSet,
        tree: &TreePath,
        query: &PageQuery,
    ) -> Result<PageInOutcome, EngineError> {
        let mut outcome = PageInOutcome::default();

        // 1. Resolve candidates (metadata only).
        let mut candidates = match self.data_plane.resolve(tree, query).await {
            Ok(c) => c,
            Err(e) => {
                error!(error = %e, tree = tree.as_str(), "data_plane.resolve failed during page_in");
                return Err(EngineError::DataPlane(e));
            }
        };
        // Importance order (high first); deterministic tie-break on address.
        candidates.sort_by(|a, b| {
            b.importance
                .partial_cmp(&a.importance)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.addr.0.cmp(&b.addr.0))
        });

        // 1b. Logical-TTL sweep BEFORE budget-pressure admission: drop every
        // resident, non-pinned page whose logical age exceeds `ws.ttl` (no-op when
        // `ws.ttl` is None/0). This is the ONLY producer of `EvictReason::Ttl`.
        self.evict_expired(ws, tree, &mut outcome).await?;

        // 2. Decide admissions, evicting to fit as needed.
        let mut to_admit: Vec<PageRef> = Vec::with_capacity(candidates.len());
        for cand in candidates {
            // Demand-hit on an already-resident page: bump frequency + recency,
            // no fetch. Recency is stamped from the DURABLE clock authority.
            if ws.pages.contains(&cand.addr) {
                let ord = self.advance_clock(ws).await?;
                if let Some(p) = ws.pages.get_mut(&cand.addr) {
                    p.use_count = p.use_count.saturating_add(1);
                    p.last_access_ord = ord;
                }
                outcome.already_resident.push(cand.addr.clone());
                continue;
            }
            // A single page larger than the whole budget can never fit.
            if cand.est_tokens > ws.budget_tokens {
                outcome.budget_exhausted.push(cand.addr.clone());
                continue;
            }
            // Make room if needed.
            if !ws.fits(cand.est_tokens) {
                let freed = self
                    .evict_to_fit(ws, tree, cand.est_tokens, &mut outcome)
                    .await?;
                if !freed {
                    // Could not free enough (only pinned remain): this and every
                    // remaining candidate cannot be admitted.
                    outcome.budget_exhausted.push(cand.addr.clone());
                    continue;
                }
            }
            // Reserve the budget now so subsequent candidates see the headroom.
            ws.resident_tokens = ws.resident_tokens.saturating_add(cand.est_tokens);
            to_admit.push(cand);
        }

        if to_admit.is_empty() {
            // Nothing to fetch; still persist (clock / use_count bumps happened).
            self.persist(ws, tree).await?;
            return Ok(outcome);
        }

        // 3. Fetch the admitted pages in bulk.
        let admit_addrs: Vec<PageAddr> = to_admit.iter().map(|r| r.addr.clone()).collect();
        let contents = match self.data_plane.get_many(tree, &admit_addrs).await {
            Ok(c) => c,
            Err(e) => {
                error!(error = %e, tree = tree.as_str(), "data_plane.get_many failed during page_in");
                // Roll back the reserved budget for the un-fetched admissions so
                // the invariant (resident_tokens == Σ est_tokens) holds.
                for r in &to_admit {
                    ws.resident_tokens = ws.resident_tokens.saturating_sub(r.est_tokens);
                }
                return Err(EngineError::DataPlane(e));
            }
        };
        // Index fetched content by address (get_many order is unspecified).
        let mut by_addr: std::collections::HashMap<&str, &str> =
            std::collections::HashMap::with_capacity(contents.len());
        for c in &contents {
            by_addr.insert(c.addr.0.as_str(), c.bytes.as_str());
        }

        // 4. Situate + insert each admitted page.
        for r in &to_admit {
            // Situate via the deterministic contextual prefix (kept for parity
            // with embedding-time contextualization; the prefix is computed and
            // attached, never an LLM call).
            let _prefix = build_context_prefix(&ChunkContext {
                relative_path: r.addr.0.clone(),
                ..ChunkContext::default()
            });
            // The situated bytes are tracked by the data plane; the control plane
            // tracks token cost + metadata. A corpus admit is re-fetchable, so its
            // `bytes` are `None` (only a `Scratch` page carries write-back bytes —
            // see `ResidentPage::bytes` and `admit_scratch`). We still bind the
            // fetched bytes to confirm the address resolved.
            let _bytes = by_addr.get(r.addr.0.as_str()).copied().unwrap_or("");
            let ord = self.advance_clock(ws).await?;
            ws.pages.insert(ResidentPage {
                addr: r.addr.clone(),
                kind: r.kind,
                importance: r.importance,
                est_tokens: r.est_tokens,
                use_count: 1,
                last_access_ord: ord,
                dirty: false,
                pinned: false,
                // Admitted from the read-only corpus reference: re-fetchable, so the
                // control plane keeps only token/importance metadata (see
                // `ResidentPage::bytes`). Scratch bytes never enter via admission.
                bytes: None,
            });
            outcome.admitted.push(r.addr.clone());
        }

        // 5. Persist.
        self.persist(ws, tree).await?;
        Ok(outcome)
    }

    /// **C3 unification write path** — admit (or update) a `Scratch` page whose
    /// bytes the control plane *owns* (an RLM accumulator fold, a REPL output, the
    /// `tape_put` tool). Unlike a corpus admit (re-fetchable, `bytes: None`), a
    /// scratch page has no corpus source, so its bytes ride on the resident row
    /// ([`ResidentPage::bytes`] = `Some`) and are persisted to
    /// `working_set_pages.content`, the only way they survive a pause/resume (see
    /// [`store::rehydrate_store_from_pages`]).
    ///
    /// Steps (mirroring [`page_in`](Self::page_in)'s accounting):
    /// 1. **Stage** `bytes` into the per-tree [`context_tape::TapeStore`] via
    ///    `data_plane.put` (marks the store page dirty — the authoritative RAM
    ///    copy).
    /// 2. **Budget**: if the page is new and does not fit, [`evict_to_fit`] makes
    ///    room (returning [`EngineError`] only on a seam/DB fault — a budget that
    ///    cannot be met because only pinned pages remain leaves the page
    ///    *un-admitted* and is reported via the returned outcome's
    ///    `budget_exhausted`). A page larger than the whole budget can never be
    ///    admitted (also `budget_exhausted`).
    /// 3. **Insert / update** the [`ResidentPage`] carrying `bytes: Some(..)`,
    ///    `est_tokens = Page::estimate_tokens(bytes)`, `dirty: true`, and the
    ///    caller's `importance`; the resident-token sum is adjusted by the exact
    ///    delta (so the invariant holds whether the addr was already resident).
    /// 4. **Clock**: stamp `last_access_ord` from the durable clock authority
    ///    ([`advance_clock`](Self::advance_clock)).
    /// 5. **Persist** the working set (writing `content` durably).
    ///
    /// The control plane has no `Scratch` [`PageKind`]; per the established
    /// convention (see [`crate::tape::real_data_plane`]'s `kind_of`) a scratch page
    /// is bucketed as [`PageKind::FileChunk`] in the control-plane row — the
    /// discriminator that it is *scratch* is `bytes.is_some()` together with its
    /// `scratch/…` address, which is exactly what [`store::rehydrate_store_from_pages`]
    /// keys on.
    ///
    /// Returns the same [`PageInOutcome`] shape as [`page_in`]: `admitted` (a new
    /// scratch page), `already_resident` (an updated one), `evicted` /
    /// `demoted_in` (anything displaced to make room), or `budget_exhausted` (the
    /// page could not be admitted within budget).
    pub async fn admit_scratch(
        &self,
        ws: &mut WorkingSet,
        tree: &TreePath,
        addr: &PageAddr,
        bytes: &str,
        importance: f32,
    ) -> Result<PageInOutcome, EngineError> {
        let mut outcome = PageInOutcome::default();
        let new_tokens = context_tape::Page::estimate_tokens(bytes) as i32;

        // 1. Stage the bytes into the per-tree store as DIRTY (authoritative RAM
        //    copy). This is the data-plane write; residency is decided below.
        self.data_plane.put(tree, addr, bytes).await.map_err(|e| {
            error!(error = %e, addr = addr.0.as_str(), "data_plane.put (scratch stage) failed");
            EngineError::DataPlane(e)
        })?;

        // Already resident? Update bytes + token delta in place (a demand-style
        // re-write), restamp recency, and persist. No eviction needed unless the
        // larger content now overflows the budget.
        if let Some(existing) = ws.pages.get(addr) {
            let old_tokens = existing.est_tokens;
            let delta = new_tokens.saturating_sub(old_tokens);
            // Grow the resident set if the new content is larger; evict to fit the
            // positive delta first so the budget bound holds.
            if delta > 0 && !ws.fits(delta) {
                let freed = self.evict_to_fit(ws, tree, delta, &mut outcome).await?;
                if !freed {
                    // Cannot grow within budget (only pinned remain). Leave the
                    // page at its OLD size — refreshing its bytes would break the
                    // token invariant — so we report it as exhausted and do not
                    // apply the larger content to the control-plane row. The
                    // data-plane stage above still holds the new bytes for the RAM
                    // copy; the control-plane row keeps consistent accounting.
                    outcome.budget_exhausted.push(addr.clone());
                    self.persist(ws, tree).await?;
                    return Ok(outcome);
                }
            }
            let ord = self.advance_clock(ws).await?;
            // `evict_to_fit` chooses victims from the unpinned resident set, which
            // includes THIS page — a degenerate policy could evict the very page we
            // are growing. Apply the delta only if it survived; otherwise fall
            // through to the fresh-insert path below so accounting stays exact (its
            // old tokens were already subtracted by its own eviction).
            if let Some(p) = ws.pages.get_mut(addr) {
                p.est_tokens = new_tokens;
                p.importance = importance;
                p.dirty = true;
                p.bytes = Some(bytes.to_string());
                p.use_count = p.use_count.saturating_add(1);
                p.last_access_ord = ord;
                ws.resident_tokens = ws.resident_tokens.saturating_add(delta);
                outcome.already_resident.push(addr.clone());
                self.persist(ws, tree).await?;
                return Ok(outcome);
            }
            // The page evicted itself to make room; re-admit it fresh below. (It is
            // recorded in `outcome.evicted`; the fresh insert re-adds it to
            // `outcome.admitted`, an accurate evict→re-admit trace.) Fall through.
        }

        // New scratch page. A page larger than the whole budget can never fit.
        if new_tokens > ws.budget_tokens {
            outcome.budget_exhausted.push(addr.clone());
            self.persist(ws, tree).await?;
            return Ok(outcome);
        }
        // 2. Make room if needed.
        if !ws.fits(new_tokens) {
            let freed = self
                .evict_to_fit(ws, tree, new_tokens, &mut outcome)
                .await?;
            if !freed {
                outcome.budget_exhausted.push(addr.clone());
                self.persist(ws, tree).await?;
                return Ok(outcome);
            }
        }
        // 3 + 4. Insert the resident row carrying the bytes, stamped from the
        //        durable clock authority.
        let ord = self.advance_clock(ws).await?;
        ws.pages.insert(ResidentPage {
            addr: addr.clone(),
            // Control plane has no Scratch kind; bucket as FileChunk per the
            // established convention. `bytes: Some` is the scratch discriminator.
            kind: PageKind::FileChunk,
            importance,
            est_tokens: new_tokens,
            use_count: 1,
            last_access_ord: ord,
            dirty: true,
            pinned: false,
            bytes: Some(bytes.to_string()),
        });
        ws.resident_tokens = ws.resident_tokens.saturating_add(new_tokens);
        outcome.admitted.push(addr.clone());

        // 5. Persist (writes `content` durably).
        self.persist(ws, tree).await?;
        Ok(outcome)
    }

    /// Evict resident pages (per policy) until `needed` tokens of headroom
    /// exist. Returns `true` if enough was freed, `false` if only pinned pages
    /// remain and the requirement cannot be met. Mutates `ws` and records each
    /// eviction (and any demotion-in) in `outcome`.
    pub async fn evict_to_fit(
        &self,
        ws: &mut WorkingSet,
        tree: &TreePath,
        needed: i32,
        outcome: &mut PageInOutcome,
    ) -> Result<bool, EngineError> {
        // Loop until the requested tokens fit or no unpinned victim remains.
        loop {
            if ws.resident_tokens.saturating_add(needed) <= ws.budget_tokens {
                return Ok(true);
            }
            // Select the next victim in a tight scope. The policy engine is a
            // stateless `Box<dyn EvictionPolicyEngine>` (now `Send + Sync`), built
            // per-iteration; candidates are recomputed each loop, so per-iteration
            // construction is semantically identical to hoisting it.
            let victim_addr = {
                let engine = policy_engine(ws.policy);
                // Gather unpinned candidates *in insertion order* (so FIFO sees
                // arrival order); the policy re-ranks as needed.
                let candidates: Vec<&ResidentPage> =
                    ws.pages.iter_in_order().filter(|p| !p.pinned).collect();
                match engine.select_victim(&candidates, ws.clock) {
                    Some(addr) => addr,
                    // Only pinned pages left — cannot free more.
                    None => return Ok(false),
                }
            };

            // By-design budget-pressure eviction (ADR-021 warn!, non-trigger
            // wording so the no_swallowed_error_warn guard does not flag it).
            warn!(
                session = ws.session_key.as_str(),
                cursor = ws.state_cursor,
                addr = victim_addr.0.as_str(),
                policy = ws.policy.as_str(),
                "context-tape: evicting page under budget pressure"
            );

            self.evict_one(ws, tree, &victim_addr, EvictReason::BudgetPressure, outcome)
                .await?;
        }
    }

    /// Logical-TTL eviction: drop every resident, **non-pinned** page whose
    /// logical age (`ws.clock − last_access_ord`) **strictly exceeds** `ws.ttl`,
    /// recording each with [`EvictReason::Ttl`]. This is the SOLE producer of the
    /// `Ttl` reason. A `None`/`0` TTL disables it (a benign no-op, returning
    /// immediately). Called at the top of [`page_in`](Self::page_in), before any
    /// budget-pressure admission, so stale pages are reclaimed independently of
    /// whether new pages need room.
    ///
    /// Expiry is measured on the **logical** clock (never wall-time), so it is a
    /// deterministic function of the replayed trace. The victim set is snapshotted
    /// against the current clock before any mutation (TTL expiry is not an access
    /// and does not advance the clock), then each victim flows through the shared
    /// [`evict_one`](Self::evict_one) path (write-back-if-dirty → demotion ladder →
    /// remove → record). Pinned pages are exempt (TTL never overrides a hard
    /// anchor). Each eviction logs at `warn!` with deliberately non-trigger
    /// wording (ADR-021: a by-design reclamation, not a runtime error).
    pub async fn evict_expired(
        &self,
        ws: &mut WorkingSet,
        tree: &TreePath,
        outcome: &mut PageInOutcome,
    ) -> Result<(), EngineError> {
        let Some(ttl) = ws.ttl.filter(|t| *t > 0) else {
            return Ok(()); // No logical TTL configured — nothing to expire.
        };
        let clock = ws.clock;
        // Snapshot the expired, unpinned victims in insertion order (deterministic)
        // before mutating; computing ages against the pre-eviction clock.
        let expired: Vec<PageAddr> = ws
            .pages
            .iter_in_order()
            .filter(|p| !p.pinned && p.logical_age(clock) > ttl)
            .map(|p| p.addr.clone())
            .collect();
        for addr in &expired {
            // Re-check residency: a prior demotion-in this loop never removes an
            // already-listed victim, but evict_one is the single safe chokepoint.
            let Some(age) = ws.pages.get(addr).map(|p| p.logical_age(clock)) else {
                continue;
            };
            // By-design TTL reclamation (ADR-021 warn!, non-trigger wording so the
            // no_swallowed_error_warn guard does not flag it).
            warn!(
                session = ws.session_key.as_str(),
                cursor = ws.state_cursor,
                addr = addr.0.as_str(),
                logical_age = age,
                ttl,
                "context-tape: expiring page past its logical TTL"
            );
            self.evict_one(ws, tree, addr, EvictReason::Ttl, outcome)
                .await?;
        }
        Ok(())
    }

    /// Evict one specific page: write-back if dirty, run the demotion ladder,
    /// remove it, and record the [`EvictReason`]. Pinned pages are refused
    /// (returns without evicting) — a safety belt; callers pre-filter pinned.
    async fn evict_one(
        &self,
        ws: &mut WorkingSet,
        tree: &TreePath,
        addr: &PageAddr,
        reason: EvictReason,
        outcome: &mut PageInOutcome,
    ) -> Result<(), EngineError> {
        let Some(page) = ws.pages.get(addr).cloned() else {
            return Ok(());
        };
        if page.pinned {
            return Ok(());
        }

        // Write-back a dirty victim (exactly once) BEFORE removal. This `put`
        // commits the page's content as a bi-temporal supersession (the real impl
        // closes the prior `valid_to` and opens a fresh `valid_from`; the mock
        // counts the call). A `Scratch` page carries its REAL bytes on the
        // resident row ([`ResidentPage::bytes`] = `Some`), so they are written
        // back verbatim — losing them would corrupt a resumed run. A corpus page
        // carries `None` (its bytes are re-fetchable / staged in the data plane);
        // the empty string is then the data plane's documented "flush staged dirty
        // content for this address" signal (it re-stages the existing content
        // rather than clobbering it — see `RealTapeDataPlane::put`).
        if page.dirty {
            let write_back = page.bytes.as_deref().unwrap_or("");
            self.data_plane.put(tree, addr, write_back).await.map_err(|e| {
                error!(error = %e, addr = addr.0.as_str(), "data_plane.put (write-back) failed");
                EngineError::DataPlane(e)
            })?;
        }

        // Remove the victim FIRST (free its tokens), then page in any demotion
        // summary into the freed space. This ordering keeps `resident_tokens`
        // monotonically within budget at every step — the summary is admitted
        // into headroom the eviction just created, never transiently over-budget.
        if let Some(removed) = ws.pages.remove(addr) {
            ws.resident_tokens = ws.resident_tokens.saturating_sub(removed.est_tokens);
        }
        store::evict_page(self.pool, &ws.session_key, ws.state_cursor, addr, reason).await?;
        outcome.evicted.push(addr.clone());

        // Demotion ladder: try to page in a compact summary standing in for the
        // just-evicted leaf (now that its tokens are freed).
        match self
            .data_plane
            .summary_of(tree, std::slice::from_ref(addr))
            .await
        {
            Ok(Some(summary)) => {
                self.try_demote_in(ws, tree, &page, &summary, outcome)
                    .await?;
            }
            Ok(None) => {
                // By-design: no stand-in available (ADR-021 warn!, non-trigger
                // wording).
                warn!(
                    addr = addr.0.as_str(),
                    "context-tape: no summary available for demotion; evicting leaf without a stand-in"
                );
            }
            Err(e) => {
                error!(error = %e, addr = addr.0.as_str(), "data_plane.summary_of failed during demotion");
                return Err(EngineError::DataPlane(e));
            }
        }
        Ok(())
    }

    /// Page in a demotion `summary` for the just-evicted `leaf`, **only** if it
    /// is a `SummaryNode`, strictly smaller than the leaf, fits in the current
    /// headroom (the leaf's tokens have already been freed by the caller), and is
    /// not already resident.
    async fn try_demote_in(
        &self,
        ws: &mut WorkingSet,
        tree: &TreePath,
        leaf: &ResidentPage,
        summary: &PageRef,
        outcome: &mut PageInOutcome,
    ) -> Result<(), EngineError> {
        if summary.kind != PageKind::SummaryNode {
            return Ok(());
        }
        if summary.est_tokens >= leaf.est_tokens {
            return Ok(()); // Not a reduction — skip.
        }
        if ws.pages.contains(&summary.addr) {
            return Ok(()); // Already resident.
        }
        // The leaf is already removed, so `resident_tokens` is post-eviction. The
        // summary fits iff `resident_tokens + summary <= budget`. Because the
        // summary is strictly smaller than the leaf it replaced, this holds
        // whenever the leaf itself was within budget; guard explicitly anyway.
        let projected = ws.resident_tokens.saturating_add(summary.est_tokens);
        if projected > ws.budget_tokens {
            return Ok(());
        }
        let content = match self.data_plane.get(tree, &summary.addr).await {
            Ok(c) => c,
            Err(e) => {
                error!(error = %e, addr = summary.addr.0.as_str(), "data_plane.get failed paging in demotion summary");
                return Err(EngineError::DataPlane(e));
            }
        };
        let ord = self.advance_clock(ws).await?;
        ws.pages.insert(ResidentPage {
            addr: summary.addr.clone(),
            kind: PageKind::SummaryNode,
            importance: summary.importance,
            est_tokens: content.est_tokens,
            use_count: 1,
            last_access_ord: ord,
            dirty: false,
            pinned: false,
            // A summary node is re-derivable from the corpus, so it carries no
            // write-back bytes (see `ResidentPage::bytes`): only Scratch pages do.
            bytes: None,
        });
        ws.resident_tokens = ws.resident_tokens.saturating_add(content.est_tokens);
        outcome.demoted_in.push(summary.addr.clone());
        Ok(())
    }

    /// Persist the working set (config + resident pages). Tree path is recorded
    /// on each page row.
    async fn persist(&self, ws: &WorkingSet, tree: &TreePath) -> Result<(), EngineError> {
        // model_window / ttl are session config not held on the in-memory set;
        // pass through neutral values (the dedicated config save owns those).
        store::save_working_set(self.pool, ws, tree.as_str(), ws.budget_tokens, None).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape::working_set::PageAddr;

    fn page(addr: &str, tokens: i32, importance: f32, use_count: u32, ord: u64) -> ResidentPage {
        ResidentPage {
            addr: PageAddr(addr.into()),
            kind: PageKind::FileChunk,
            importance,
            est_tokens: tokens,
            use_count,
            last_access_ord: ord,
            dirty: false,
            pinned: false,
            bytes: None,
        }
    }

    // The policy engines are pure (no DB / data-plane), so they are unit-tested
    // directly here. The DB-backed page_in / evict paths are covered by the
    // integration tests in `pgmcp-testing` against `MockTapeDataPlane`.

    #[test]
    fn importance_weighted_keeps_high_importance_drops_low_lru() {
        // The discriminating test: a high-importance page that is also the LRU
        // (oldest) must NOT be the victim; a low-importance page is chosen
        // instead.
        let hi = page("hi", 10, 100.0, 1, 0); // oldest, but very important
        let lo = page("lo", 10, 0.1, 1, 5); // newer, but unimportant
        let cands = [&hi, &lo];
        let victim = ImportanceWeightedPolicy.select_victim(&cands, 10);
        assert_eq!(
            victim,
            Some(PageAddr("lo".into())),
            "low-importance evicted"
        );
    }

    #[test]
    fn lru_picks_oldest_last_access() {
        let a = page("a", 1, 1.0, 1, 1);
        let b = page("b", 1, 1.0, 1, 9);
        let c = page("c", 1, 1.0, 1, 5);
        let cands = [&a, &b, &c];
        assert_eq!(
            LruPolicy.select_victim(&cands, 10),
            Some(PageAddr("a".into())),
            "oldest last_access_ord is the LRU victim"
        );
    }

    #[test]
    fn lfu_picks_least_used() {
        let a = page("a", 1, 1.0, 5, 1);
        let b = page("b", 1, 1.0, 1, 9); // least used
        let c = page("c", 1, 1.0, 3, 5);
        let cands = [&a, &b, &c];
        assert_eq!(
            LfuPolicy.select_victim(&cands, 10),
            Some(PageAddr("b".into())),
            "smallest use_count is the LFU victim"
        );
    }

    #[test]
    fn fifo_picks_first_inserted() {
        // Candidates are supplied in insertion order; FIFO evicts the front.
        let a = page("a", 1, 1.0, 9, 9);
        let b = page("b", 1, 1.0, 1, 1);
        let cands = [&a, &b]; // 'a' inserted first
        assert_eq!(
            FifoPolicy.select_victim(&cands, 10),
            Some(PageAddr("a".into())),
            "first-inserted is the FIFO victim regardless of recency/frequency"
        );
    }

    #[test]
    fn ttl_picks_greatest_logical_age() {
        let a = page("a", 1, 1.0, 1, 2); // age 8
        let b = page("b", 1, 1.0, 1, 9); // age 1
        let cands = [&a, &b];
        assert_eq!(
            TtlPolicy.select_victim(&cands, 10),
            Some(PageAddr("a".into())),
            "greatest logical age (clock - last_access_ord) is the TTL victim"
        );
    }

    #[test]
    fn cost_aware_picks_largest_oldest_rarely_used() {
        let small_new = page("small", 1, 1.0, 5, 9); // low cost
        let big_old = page("big", 100, 1.0, 1, 0); // high cost
        let cands = [&small_new, &big_old];
        assert_eq!(
            CostAwarePolicy.select_victim(&cands, 10),
            Some(PageAddr("big".into())),
            "largest (age x tokens)/(use+1) is the cost-aware victim"
        );
    }

    #[test]
    fn no_victim_among_empty() {
        let cands: [&ResidentPage; 0] = [];
        for p in [
            EvictionPolicy::Lru,
            EvictionPolicy::Lfu,
            EvictionPolicy::Ttl,
            EvictionPolicy::Fifo,
            EvictionPolicy::CostAware,
            EvictionPolicy::ImportanceWeighted,
        ] {
            assert_eq!(policy_engine(p).select_victim(&cands, 0), None);
        }
    }

    // ----------------------------------------------------------------------
    // DB-backed engine tests (TTL eviction + admit_scratch). These exercise the
    // persisted residency paths (bump_clock / evict_page / save_working_set), so
    // they need a pool; they SKIP when no DB is reachable (self-contained — see
    // the same rationale in `crate::tape::store`'s test module). The data plane is
    // the in-memory `MockTapeDataPlane` (a full contract impl, not a stub).
    // ----------------------------------------------------------------------

    use crate::tape::data_plane::{MockTapeDataPlane, TreePath};

    async fn engine_test_pool() -> Option<PgPool> {
        use sqlx::postgres::PgPoolOptions;
        // ONLY an isolated test DB (`PGMCP_TEST_DATABASE_URL`) — NEVER the live
        // default DB: these tests mutate working-set rows and run migrations, which
        // must not touch a production corpus (and the live DB may carry a 765k-row
        // memory_unified matview whose rebuild would be ruinous here). Skip cleanly
        // when unset.
        let url = std::env::var("PGMCP_TEST_DATABASE_URL").ok()?;
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .ok()?;
        // Ensure the schema matches the code (idempotent; brings v51/v52/v53 — the
        // working_set tables, the `content` column, and the relaxed session FK — up
        // to date) so these DB-backed tests are self-contained against any reachable
        // isolated test DB rather than depending on its prior migration state.
        crate::db::migrations::run_migrations(
            &pool,
            &crate::config::VectorConfig::default(),
            false,
        )
        .await
        .ok()?;
        Some(pool)
    }

    async fn purge(pool: &PgPool, session_key: &str) {
        let _ = sqlx::query("DELETE FROM working_set_pages WHERE session_key = $1")
            .bind(session_key)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM working_set_config WHERE session_key = $1")
            .bind(session_key)
            .execute(pool)
            .await;
    }

    fn fresh_key() -> String {
        format!("rlm:{}", uuid::Uuid::new_v4())
    }

    /// TTL: pages whose logical age exceeds `ws.ttl` are evicted with
    /// `EvictReason::Ttl`; a `ttl = None` working set expires nothing.
    #[tokio::test]
    async fn evict_expired_drops_only_stale_pages_with_ttl_reason() {
        let Some(pool) = engine_test_pool().await else {
            eprintln!("[tape::engine] SKIPPED evict_expired: no test DB");
            return;
        };
        let session_key = fresh_key();
        purge(&pool, &session_key).await;
        let dp = MockTapeDataPlane::new();
        let engine = PagingEngine::new(&pool, &dp);
        let tree = TreePath(session_key.clone());

        // Working set at clock=10, ttl=3. Pages: a(age 8 → expired),
        // b(age 1 → fresh), c(age 5 → expired but pinned → exempt).
        let mut ws = WorkingSet::new(session_key.clone(), 0, 1000, EvictionPolicy::Lru);
        ws.ttl = Some(3);
        ws.clock = 10;
        let mut a = page("scratch/a", 10, 1.0, 1, 2); // age 8
        a.bytes = Some("aaa".into());
        let b = page("scratch/b", 10, 1.0, 1, 9); // age 1
        let mut c = page("scratch/c", 10, 1.0, 1, 5); // age 5
        c.pinned = true;
        ws.pages.insert(a);
        ws.pages.insert(b);
        ws.pages.insert(c);
        ws.resident_tokens = ws.recompute_resident_tokens();
        // Seed the durable config (initial INSERT seeds logical_clock from
        // ws.clock = 10). `evict_expired` computes ages against the in-memory
        // `ws.clock`, not the durable value, and does NOT advance the clock (TTL
        // expiry is not an access), so this single seed is sufficient.
        store::save_config(&pool, &ws, 1000, Some(3))
            .await
            .expect("seed config");

        let mut outcome = PageInOutcome::default();
        engine
            .evict_expired(&mut ws, &tree, &mut outcome)
            .await
            .expect("evict_expired");

        // a is expired+unpinned → evicted; b fresh → kept; c pinned → kept.
        assert!(
            outcome.evicted.contains(&PageAddr("scratch/a".into())),
            "stale unpinned page a evicted"
        );
        assert!(!ws.pages.contains(&PageAddr("scratch/a".into())), "a gone");
        assert!(
            ws.pages.contains(&PageAddr("scratch/b".into())),
            "fresh b kept"
        );
        assert!(
            ws.pages.contains(&PageAddr("scratch/c".into())),
            "pinned c exempt from TTL"
        );
        // The durable row records the Ttl reason.
        let reason: Option<String> = sqlx::query_scalar(
            "SELECT evict_reason FROM working_set_pages
              WHERE session_key = $1 AND page_addr = 'scratch/a'",
        )
        .bind(&session_key)
        .fetch_one(&pool)
        .await
        .expect("read reason");
        assert_eq!(reason.as_deref(), Some("ttl"), "EvictReason::Ttl recorded");

        // With ttl = None, a second pass expires nothing.
        ws.ttl = None;
        let mut outcome2 = PageInOutcome::default();
        engine
            .evict_expired(&mut ws, &tree, &mut outcome2)
            .await
            .expect("evict_expired none");
        assert!(outcome2.evicted.is_empty(), "ttl=None expires nothing");

        purge(&pool, &session_key).await;
    }

    /// `admit_scratch` writes a scratch page carrying its bytes, persists them to
    /// `content`, advances the durable clock, and stays within budget.
    #[tokio::test]
    async fn admit_scratch_persists_bytes_and_respects_budget() {
        let Some(pool) = engine_test_pool().await else {
            eprintln!("[tape::engine] SKIPPED admit_scratch: no test DB");
            return;
        };
        let session_key = fresh_key();
        purge(&pool, &session_key).await;
        let dp = MockTapeDataPlane::new();
        let engine = PagingEngine::new(&pool, &dp);
        let tree = TreePath(session_key.clone());

        let mut ws = WorkingSet::new(session_key.clone(), 0, 1000, EvictionPolicy::Lru);
        let addr = PageAddr("scratch/accum-0".into());
        let payload = "fold-result: 0+1+2+3 = 6 (situated accumulator output)";

        let outcome = engine
            .admit_scratch(&mut ws, &tree, &addr, payload, 0.9)
            .await
            .expect("admit_scratch");
        assert!(outcome.admitted.contains(&addr), "scratch page admitted");
        assert!(ws.pages.contains(&addr), "resident after admit");
        // The in-memory resident page carries the bytes + dirty + importance.
        let resident = ws.pages.get(&addr).expect("resident");
        assert_eq!(resident.bytes.as_deref(), Some(payload));
        assert!(
            resident.dirty,
            "scratch page is dirty (owns unflushed bytes)"
        );
        assert!((resident.importance - 0.9).abs() < f32::EPSILON);
        // The token invariant holds.
        assert_eq!(ws.resident_tokens, ws.recompute_resident_tokens());
        assert!(ws.resident_tokens <= ws.budget_tokens);

        // The durable content column holds the bytes; the clock advanced past 0.
        let content: Option<String> = sqlx::query_scalar(
            "SELECT content FROM working_set_pages WHERE session_key = $1 AND page_addr = $2",
        )
        .bind(&session_key)
        .bind(&addr.0)
        .fetch_one(&pool)
        .await
        .expect("read content");
        assert_eq!(
            content.as_deref(),
            Some(payload),
            "bytes persisted to content"
        );
        let clock: i64 = sqlx::query_scalar(
            "SELECT logical_clock FROM working_set_config WHERE session_key = $1",
        )
        .bind(&session_key)
        .fetch_one(&pool)
        .await
        .expect("read clock");
        assert!(clock >= 1, "admit_scratch advanced the durable clock");

        // A second admit_scratch at the SAME address updates in place (already
        // resident), refreshing bytes without leaking tokens.
        let payload2 = "fold-result (revised): 10";
        let outcome2 = engine
            .admit_scratch(&mut ws, &tree, &addr, payload2, 0.5)
            .await
            .expect("admit_scratch update");
        assert!(
            outcome2.already_resident.contains(&addr),
            "re-admit updates in place"
        );
        assert_eq!(
            ws.pages.get(&addr).expect("resident").bytes.as_deref(),
            Some(payload2)
        );
        assert_eq!(ws.resident_tokens, ws.recompute_resident_tokens());

        purge(&pool, &session_key).await;
    }

    // ----------------------------------------------------------------------
    // Property: the resident-token invariant + budget bound + pinned-safety
    // hold across an arbitrary interleaving of page-in / evict operations,
    // under every policy. This is a DB-free model that reproduces the engine's
    // EXACT token accounting and eviction loop (admit reserves tokens, evict
    // frees the victim's tokens, pinned never selected), so it pins the
    // mechanical invariants independently of Postgres / the data plane.
    // ----------------------------------------------------------------------

    use proptest::prelude::*;

    /// A scripted operation in the property model.
    #[derive(Debug, Clone)]
    enum Op {
        /// Try to admit a page (addr index, tokens, importance, pinned).
        PageIn {
            id: u16,
            tokens: i32,
            importance: f32,
            pinned: bool,
        },
        /// Force one eviction pass (no-op if nothing unpinned is resident).
        EvictOne,
    }

    /// Pure model of `page_in` for ONE candidate + `evict_to_fit`, mirroring the
    /// engine's accounting without persistence/data-plane. Returns the updated
    /// working set. Pinned pages are never evicted; the budget is never exceeded.
    fn model_admit(ws: &mut WorkingSet, op: &Op) {
        match op {
            Op::PageIn {
                id,
                tokens,
                importance,
                pinned,
            } => {
                let addr = PageAddr(format!("p{id}"));
                let tokens = (*tokens).clamp(1, ws.budget_tokens.max(1));
                // Demand-hit: restamp, bump use_count, no token change.
                if ws.pages.contains(&addr) {
                    let ord = ws.tick();
                    if let Some(p) = ws.pages.get_mut(&addr) {
                        p.use_count = p.use_count.saturating_add(1);
                        p.last_access_ord = ord;
                    }
                    return;
                }
                // A page bigger than the whole budget can never be admitted.
                if tokens > ws.budget_tokens {
                    return;
                }
                // Evict to fit (same loop as the engine).
                let engine = policy_engine(ws.policy);
                while ws.resident_tokens.saturating_add(tokens) > ws.budget_tokens {
                    let cands: Vec<&ResidentPage> =
                        ws.pages.iter_in_order().filter(|p| !p.pinned).collect();
                    let Some(victim) = engine.select_victim(&cands, ws.clock) else {
                        return; // only pinned remain → cannot admit
                    };
                    if let Some(removed) = ws.pages.remove(&victim) {
                        ws.resident_tokens = ws.resident_tokens.saturating_sub(removed.est_tokens);
                    }
                }
                let ord = ws.tick();
                ws.pages.insert(ResidentPage {
                    addr,
                    kind: PageKind::FileChunk,
                    importance: importance.max(0.0),
                    est_tokens: tokens,
                    use_count: 1,
                    last_access_ord: ord,
                    dirty: false,
                    pinned: *pinned,
                    bytes: None,
                });
                ws.resident_tokens = ws.resident_tokens.saturating_add(tokens);
            }
            Op::EvictOne => {
                let engine = policy_engine(ws.policy);
                let cands: Vec<&ResidentPage> =
                    ws.pages.iter_in_order().filter(|p| !p.pinned).collect();
                if let Some(victim) = engine.select_victim(&cands, ws.clock)
                    && let Some(removed) = ws.pages.remove(&victim)
                {
                    ws.resident_tokens = ws.resident_tokens.saturating_sub(removed.est_tokens);
                }
            }
        }
    }

    fn policy_strategy() -> impl Strategy<Value = EvictionPolicy> {
        prop_oneof![
            Just(EvictionPolicy::Lru),
            Just(EvictionPolicy::Lfu),
            Just(EvictionPolicy::Ttl),
            Just(EvictionPolicy::Fifo),
            Just(EvictionPolicy::CostAware),
            Just(EvictionPolicy::ImportanceWeighted),
        ]
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0u16..6, 1i32..40, 0.0f32..10.0, any::<bool>()).prop_map(
                |(id, tokens, importance, pinned)| Op::PageIn {
                    id,
                    tokens,
                    importance,
                    pinned,
                }
            ),
            Just(Op::EvictOne),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]

        /// Across any op sequence under any policy: (1) the running token sum
        /// equals Σ over resident pages; (2) it never exceeds the budget; (3)
        /// every pinned page that was admitted is still resident (never evicted).
        #[test]
        fn token_invariant_and_budget_and_pinned_hold(
            policy in policy_strategy(),
            budget in 20i32..200,
            ops in proptest::collection::vec(op_strategy(), 0..40),
        ) {
            let mut ws = WorkingSet::new("prop", 0, budget, policy);
            // Track which page ids were admitted as pinned (and still small enough
            // to be admittable) so we can assert they survive.
            let mut ever_pinned: std::collections::HashSet<String> =
                std::collections::HashSet::new();

            for op in &ops {
                // Note pinned admissions that can actually be admitted.
                if let Op::PageIn { id, tokens, pinned: true, .. } = op {
                    let t = (*tokens).clamp(1, budget.max(1));
                    if t <= budget {
                        // It may still fail to admit if only pinned pages block it,
                        // but once admitted it must never be evicted.
                        let addr = format!("p{id}");
                        let was_resident_before = ws.pages.contains(&PageAddr(addr.clone()));
                        model_admit(&mut ws, op);
                        if !was_resident_before && ws.pages.contains(&PageAddr(addr.clone())) {
                            ever_pinned.insert(addr);
                        }
                        // Invariants after each step.
                        prop_assert_eq!(ws.resident_tokens, ws.recompute_resident_tokens());
                        prop_assert!(ws.resident_tokens <= ws.budget_tokens);
                        continue;
                    }
                }
                model_admit(&mut ws, op);
                prop_assert_eq!(
                    ws.resident_tokens,
                    ws.recompute_resident_tokens(),
                    "resident_tokens must equal the sum of est_tokens"
                );
                prop_assert!(
                    ws.resident_tokens <= ws.budget_tokens,
                    "resident_tokens {} exceeded budget {}",
                    ws.resident_tokens,
                    ws.budget_tokens
                );
            }

            // Every page admitted as pinned is still resident (pinned-safety).
            for addr in &ever_pinned {
                prop_assert!(
                    ws.pages.contains(&PageAddr(addr.clone())),
                    "pinned page {} was evicted",
                    addr
                );
            }
        }
    }
}
