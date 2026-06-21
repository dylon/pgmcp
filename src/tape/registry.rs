//! The **tape-store registry**: one [`context_tape::TapeStore`] per recursion
//! tree (`TreeId == RlmFrame.root_task_id`).
//!
//! ## Why per-tree
//!
//! A `TapeStore` is the hot (in-RAM) + out-of-core tier of the context tape for
//! one orchestration run. `Scratch` pages are keyed by the tree id, and two
//! concurrent runs must never collide in the backing store — so residency is
//! sharded by `TreeId`. The registry is the shared home for those per-tree
//! stores, held on [`crate::context::SystemContext`] exactly as the per-project
//! `PhoneticsRegistry` / per-session `tool_sessions` registries are.
//!
//! ## Concurrency model
//!
//! [`context_tape::TapeStore`] owns a [`pathmap::PathMap`] and a
//! [`context_tape::AddressIndex`]; it is `Send` but **not** `Sync` (the trie and
//! index are not designed for concurrent mutation). The registry therefore wraps
//! each store in a [`std::sync::Mutex`] and keys them in a
//! [`dashmap::DashMap`] (already a pgmcp dependency). `DashMap` shards the *map*
//! so two trees' stores are reached without contending on a global lock; the
//! per-store `Mutex` serialises mutation of a *single* tree's tape (the common
//! case is a single orchestration thread touching its own tree anyway). A
//! poisoned per-store lock (a prior holder panicked) is surfaced via
//! `.expect(...)` — a panic mid-mutation has already corrupted that tree's
//! residency, so failing loudly is correct.
//!
//! ## Lifecycle
//!
//! Stores are **lazily** created on first touch ([`TapeRegistry::with_store`] /
//! [`TapeRegistry::with_store_mut`]). [`TapeRegistry::drop_tree`] finalises one
//! tree (run completion); [`TapeRegistry::reap_idle`] is the documented seam for
//! a future TTL reaper that evicts whole idle trees under memory pressure (not
//! yet wired to a cron — it is a pure, side-effect-free helper today).
//!
//! ## Trust boundary
//!
//! This is pure coordination state: it holds agent/engine working-set pages in
//! RAM (+ optional spill files the data plane configures). It never reads or
//! writes the user's source files and never touches the read-only corpus tables
//! — the corpus is read exclusively by [`crate::tape::hydrate`].

use std::sync::Mutex;
use std::time::Instant;

use dashmap::DashMap;

use context_tape::{TapeStore, TreeId};

/// Default per-tree hot-tier byte budget used when the registry lazily
/// instantiates a store and the caller supplies no explicit budget. This sizes
/// only the store's *side tables* (the dirty set + index id-tables) via
/// [`context_tape::TapeStore::with_capacity`]; the `PathMap` body itself grows
/// lazily. 8 MiB is a conservative working-set hint — generous enough to avoid
/// repeated side-table growth on a busy tree, small enough that an idle tree
/// costs little.
pub const DEFAULT_TREE_BUDGET_BYTES: usize = 8 * 1024 * 1024;

/// One registry entry: the per-tree store behind a `Mutex`, plus the wall-clock
/// instant it was last accessed (for the idle-reaper seam). `last_touched` is
/// *wall-time* deliberately — it governs **resource reclamation**, not residency
/// (residency is a function of the logical clock and never reads wall-time). So
/// it does not enter any replay-determined decision.
struct TreeEntry {
    store: Mutex<TapeStore>,
    last_touched: Mutex<Instant>,
}

impl TreeEntry {
    fn new(tree: TreeId, budget_bytes: usize) -> Self {
        Self {
            store: Mutex::new(TapeStore::with_capacity(tree, budget_bytes)),
            last_touched: Mutex::new(Instant::now()),
        }
    }

    fn touch(&self) {
        *self
            .last_touched
            .lock()
            .expect("tape registry last_touched mutex poisoned (a prior holder panicked)") =
            Instant::now();
    }
}

/// A per-[`TreeId`] registry of [`context_tape::TapeStore`]s. Cheap to clone
/// is **not** a goal — it is held by `Arc` on [`crate::context::SystemContext`];
/// callers borrow it.
pub struct TapeRegistry {
    trees: DashMap<TreeId, TreeEntry>,
    /// Byte budget every lazily-created store is sized for.
    budget_bytes: usize,
}

impl TapeRegistry {
    /// A registry whose lazily-created stores use [`DEFAULT_TREE_BUDGET_BYTES`].
    pub fn new() -> Self {
        Self::with_budget(DEFAULT_TREE_BUDGET_BYTES)
    }

    /// A registry whose lazily-created stores are sized for `budget_bytes` of
    /// side-table capacity (see [`DEFAULT_TREE_BUDGET_BYTES`]).
    pub fn with_budget(budget_bytes: usize) -> Self {
        Self {
            trees: DashMap::new(),
            budget_bytes: budget_bytes.max(1),
        }
    }

    /// Number of trees currently holding a store (test / introspection helper).
    pub fn tree_count(&self) -> usize {
        self.trees.len()
    }

    /// Whether a store exists for `tree` (without creating one).
    pub fn contains(&self, tree: &TreeId) -> bool {
        self.trees.contains_key(tree)
    }

    /// Run `f` against the per-tree store under a **shared read** of its `Mutex`,
    /// lazily creating the store on first touch. The closure receives `&TapeStore`
    /// (hot-tier / OOC reads, dirty enumeration, slice scans). Updates the entry's
    /// `last_touched` stamp.
    ///
    /// The lazy insert is confined to a **short exclusive scope** (the
    /// [`DashMap`] shard write-guard from `entry(..)` is dropped at the end of
    /// that block) before a cheap shared `get(..)` re-acquires the entry and the
    /// per-store `Mutex` is locked for `f`. The shard guard is therefore **not**
    /// held across `f`: trees on the same shard run their closures concurrently
    /// (only the brief existence-ensuring insert is exclusive).
    ///
    /// `f` must NOT re-enter the registry for a key in the **same `DashMap`
    /// shard** (only a shared shard read is held across it, so a same-shard write
    /// — i.e. a lazy insert of a not-yet-present sibling key — would deadlock).
    pub fn with_store<R>(&self, tree: TreeId, f: impl FnOnce(&TapeStore) -> R) -> R {
        // Brief exclusive insert: ensure the entry exists, then drop the shard
        // write-guard at the close of this block before locking the inner Mutex.
        {
            self.trees
                .entry(tree)
                .or_insert_with(|| TreeEntry::new(tree, self.budget_bytes));
        }
        // Re-acquire a cheap *shared* shard read for the duration of `f`.
        let entry = self.trees.get(&tree).expect(
            "tape registry entry vanished between insert and get (no concurrent drop_tree)",
        );
        entry.touch();
        let guard = entry
            .store
            .lock()
            .expect("tape store mutex poisoned (a prior holder panicked mid-mutation)");
        f(&guard)
    }

    /// Run `f` against the per-tree store under an **exclusive** lock of its
    /// `Mutex`, lazily creating the store on first touch. The closure receives
    /// `&mut TapeStore` (insert/hydrate/put/remove/spill). Updates `last_touched`.
    ///
    /// As with [`with_store`](Self::with_store), the lazy insert is confined to a
    /// short exclusive scope; the [`DashMap`] shard guard held across `f` is only
    /// a **shared** read (the inner per-store `Mutex` provides the exclusivity the
    /// `&mut TapeStore` closure needs), so trees on the same shard do not serialise
    /// on the map.
    ///
    /// `f` must NOT re-enter the registry for a key in the **same `DashMap`
    /// shard** (only a shared shard read is held across it, so a same-shard write
    /// — i.e. a lazy insert of a not-yet-present sibling key — would deadlock).
    pub fn with_store_mut<R>(&self, tree: TreeId, f: impl FnOnce(&mut TapeStore) -> R) -> R {
        // Brief exclusive insert: ensure the entry exists, then drop the shard
        // write-guard at the close of this block before locking the inner Mutex.
        {
            self.trees
                .entry(tree)
                .or_insert_with(|| TreeEntry::new(tree, self.budget_bytes));
        }
        // Re-acquire a cheap *shared* shard read for the duration of `f`.
        let entry = self.trees.get(&tree).expect(
            "tape registry entry vanished between insert and get (no concurrent drop_tree)",
        );
        entry.touch();
        let mut guard = entry
            .store
            .lock()
            .expect("tape store mutex poisoned (a prior holder panicked mid-mutation)");
        f(&mut guard)
    }

    /// Finalise a tree: drop its store, freeing its hot-tier RAM (and dropping
    /// the handle to any spill segments — the OS reclaims the mmaps when the last
    /// reference goes). Returns `true` if a store was present. Called at run
    /// completion (the orchestrator's RLM frame for `tree` finished).
    pub fn drop_tree(&self, tree: &TreeId) -> bool {
        self.trees.remove(tree).is_some()
    }

    /// **TTL-reaper seam.** Drop every tree whose store has not been touched for
    /// at least `idle_for`, returning the dropped tree ids. Intended to be driven
    /// by a future periodic cron (e.g. a `[tape] reaper_interval_secs`) under
    /// memory pressure; it is a pure helper today with no scheduler attached.
    ///
    /// Reclamation is keyed on the wall-clock `last_touched` stamp, which is NOT
    /// part of any replay-determined residency decision — so reaping a tree only
    /// frees RAM, it never changes the logically-determined working set a resumed
    /// session reconstructs (that lives in `working_set_pages`, untouched here).
    pub fn reap_idle(&self, idle_for: std::time::Duration) -> Vec<TreeId> {
        let now = Instant::now();
        let mut reaped = Vec::new();
        // Collect first (do not mutate the DashMap while iterating it).
        let stale: Vec<TreeId> = self
            .trees
            .iter()
            .filter(|e| {
                let last = *e
                    .value()
                    .last_touched
                    .lock()
                    .expect("tape registry last_touched mutex poisoned");
                now.duration_since(last) >= idle_for
            })
            .map(|e| *e.key())
            .collect();
        for tree in stale {
            if self.trees.remove(&tree).is_some() {
                reaped.push(tree);
            }
        }
        reaped
    }
}

impl Default for TapeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use context_tape::{Page, PageAddress, PageKind, PageMeta};

    fn tree_a() -> TreeId {
        uuid::Uuid::from_u128(0xa)
    }
    fn tree_b() -> TreeId {
        uuid::Uuid::from_u128(0xb)
    }

    fn scratch(tree: TreeId, slot: u8) -> (PageAddress, Page) {
        let addr = PageAddress::Scratch {
            tree,
            slot: Box::new([slot]),
        };
        let page = Page::new(
            addr.clone(),
            format!("scratch-{slot}"),
            PageMeta::clean(PageKind::Scratch, 1, 0.0),
        );
        (addr, page)
    }

    #[test]
    fn lazily_creates_one_store_per_tree() {
        let reg = TapeRegistry::new();
        assert_eq!(reg.tree_count(), 0);
        assert!(!reg.contains(&tree_a()));

        // First touch of tree A creates its store.
        let len = reg.with_store(tree_a(), |s| s.len());
        assert_eq!(len, 0, "fresh store is empty");
        assert_eq!(reg.tree_count(), 1);
        assert!(reg.contains(&tree_a()));

        // A second touch of the SAME tree reuses the store (no new entry).
        reg.with_store(tree_a(), |s| assert_eq!(s.len(), 0));
        assert_eq!(reg.tree_count(), 1);

        // A different tree gets its own store.
        reg.with_store(tree_b(), |s| assert_eq!(s.len(), 0));
        assert_eq!(reg.tree_count(), 2);
    }

    #[test]
    fn stores_are_isolated_per_tree() {
        let reg = TapeRegistry::new();
        let (addr_a, page_a) = scratch(tree_a(), 1);
        reg.with_store_mut(tree_a(), |s| {
            s.put(addr_a.clone(), page_a);
        });
        // tree A sees its page…
        reg.with_store(tree_a(), |s| assert!(s.contains(&addr_a)));
        // …tree B does not (isolation).
        reg.with_store(tree_b(), |s| assert!(!s.contains(&addr_a)));
    }

    #[test]
    fn drop_tree_frees_the_store() {
        let reg = TapeRegistry::new();
        reg.with_store(tree_a(), |_| {});
        assert!(reg.contains(&tree_a()));
        assert!(
            reg.drop_tree(&tree_a()),
            "dropping a present tree returns true"
        );
        assert!(!reg.contains(&tree_a()));
        assert!(
            !reg.drop_tree(&tree_a()),
            "dropping an absent tree returns false"
        );
    }

    #[test]
    fn reap_idle_drops_only_stale_trees() {
        let reg = TapeRegistry::new();
        reg.with_store(tree_a(), |_| {});
        // Immediately reaping with a long idle window keeps everything.
        assert!(
            reg.reap_idle(std::time::Duration::from_secs(3600))
                .is_empty(),
            "freshly-touched tree is not reaped"
        );
        assert_eq!(reg.tree_count(), 1);
        // Reaping with a zero idle window reaps the (already-aged-past-0) tree.
        let reaped = reg.reap_idle(std::time::Duration::ZERO);
        assert_eq!(reaped, vec![tree_a()]);
        assert_eq!(reg.tree_count(), 0);
    }
}
