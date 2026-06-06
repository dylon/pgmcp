//! Bounded, mtime-coherent cache of open per-project fuzzy trie handles.
//!
//! Each `PersistentARTrieChar` (inside a [`FuzzyIndex`]) spawns up to three
//! background daemon threads (`wal-sync`, `artrie-eviction`,
//! `artrie-memory-monitor`). Opening a fresh handle on every fuzzy MCP call
//! therefore churned those threads on every query (and, before the
//! libdictenstein lifecycle fix, leaked them). This cache keeps long-lived
//! handles keyed by project slug so repeated queries reuse one handle.
//!
//! **Coherence.** The `fuzzy-sync` cron rewrites each on-disk `.artrie` file
//! (~every 30 min), bumping its mtime. The cache stamps each entry with the
//! file's mtime at open time and treats an mtime change as a miss, so a cached
//! handle is transparently reopened after the cron refreshes the file. This
//! keeps the cache exactly as fresh as the file (which only the cron changes)
//! with no coupling to the cron. Per-project entries are keyed by the
//! collision-free artifact key (`slugified-name-p<project_id>`), not by display
//! name alone.
//!
//! **Bound.** At most `capacity` handles per kind (symbols, paths, concepts). On
//! overflow the least-recently-accessed entry is dropped — its daemon threads are
//! then joined by `PersistentARTrieChar::Drop`. In practice the working set is the
//! handful of projects queried within a cron interval. Only symbols, paths, and
//! concepts are cached: they are the kinds the query path opens (`fuzzy_*_search`
//! and `ontology_search`); commits and durable mandates are built by the cron
//! only. The concept trie is workspace-global (one handle under a fixed slug).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use dashmap::DashMap;
use libdictenstein::DictionaryValue;

use super::persistent_artrie::FuzzyIndex;
use super::values::{ConceptValue, PathValue, SymbolValue};

/// Max cached handles per kind. Bounds steady-state trie daemon threads to
/// ~3 × this per kind. Comfortably above a typical active working set; the
/// cron's mtime bumps keep entries fresh and unused entries are evicted
/// LRU-style, so this is a safety ceiling rather than a tuning knob.
const DEFAULT_CAPACITY: usize = 32;

struct Entry<V>
where
    V: DictionaryValue + Clone + Send + Sync + 'static,
{
    idx: Arc<FuzzyIndex<V>>,
    /// `.artrie` mtime when this handle was opened. An mtime change means the
    /// cron rebuilt the file, so the cached handle is stale and must reopen.
    mtime: Option<SystemTime>,
    /// Monotonic access stamp for approximate-LRU eviction.
    last_access: AtomicU64,
}

/// Open-`FuzzyIndex` cache for the symbol, path, and (workspace-global) concept
/// tries.
pub struct FuzzyCache {
    symbols: DashMap<String, Entry<SymbolValue>>,
    paths: DashMap<String, Entry<PathValue>>,
    concepts: DashMap<String, Entry<ConceptValue>>,
    capacity: usize,
    clock: AtomicU64,
}

impl Default for FuzzyCache {
    fn default() -> Self {
        Self::new()
    }
}

impl FuzzyCache {
    /// Cache with the default per-kind capacity.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Cache with an explicit per-kind capacity (at least 1).
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            symbols: DashMap::new(),
            paths: DashMap::new(),
            concepts: DashMap::new(),
            capacity: capacity.max(1),
            clock: AtomicU64::new(0),
        }
    }

    /// Return a cached symbol handle iff present AND the `.artrie` at `path` is
    /// unchanged since the handle was opened.
    pub fn get_symbols(&self, slug: &str, path: &Path) -> Option<Arc<FuzzyIndex<SymbolValue>>> {
        get(&self.symbols, &self.clock, slug, path)
    }

    /// Insert/replace the symbol handle for `slug`, stamping it with `path`'s
    /// current mtime, evicting LRU if over capacity. Returns the shared handle.
    pub fn insert_symbols(
        &self,
        slug: &str,
        path: &Path,
        idx: FuzzyIndex<SymbolValue>,
    ) -> Arc<FuzzyIndex<SymbolValue>> {
        insert(&self.symbols, &self.clock, self.capacity, slug, path, idx)
    }

    /// Mirror of [`get_symbols`](Self::get_symbols) for the path trie.
    pub fn get_paths(&self, slug: &str, path: &Path) -> Option<Arc<FuzzyIndex<PathValue>>> {
        get(&self.paths, &self.clock, slug, path)
    }

    /// Mirror of [`insert_symbols`](Self::insert_symbols) for the path trie.
    pub fn insert_paths(
        &self,
        slug: &str,
        path: &Path,
        idx: FuzzyIndex<PathValue>,
    ) -> Arc<FuzzyIndex<PathValue>> {
        insert(&self.paths, &self.clock, self.capacity, slug, path, idx)
    }

    /// Mirror of [`get_symbols`](Self::get_symbols) for the workspace-global
    /// concept trie (cache key is a fixed slug — see `open_concept_trie`).
    pub fn get_concepts(&self, slug: &str, path: &Path) -> Option<Arc<FuzzyIndex<ConceptValue>>> {
        get(&self.concepts, &self.clock, slug, path)
    }

    /// Mirror of [`insert_symbols`](Self::insert_symbols) for the concept trie.
    pub fn insert_concepts(
        &self,
        slug: &str,
        path: &Path,
        idx: FuzzyIndex<ConceptValue>,
    ) -> Arc<FuzzyIndex<ConceptValue>> {
        insert(&self.concepts, &self.clock, self.capacity, slug, path, idx)
    }

    /// Number of currently-cached handles (symbols + paths + concepts). For
    /// tests/metrics.
    pub fn len(&self) -> usize {
        self.symbols.len() + self.paths.len() + self.concepts.len()
    }

    /// Whether the cache holds no handles.
    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty() && self.paths.is_empty() && self.concepts.is_empty()
    }
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn get<V>(
    map: &DashMap<String, Entry<V>>,
    clock: &AtomicU64,
    slug: &str,
    path: &Path,
) -> Option<Arc<FuzzyIndex<V>>>
where
    V: DictionaryValue + Clone + Send + Sync + 'static,
{
    let mtime = file_mtime(path);
    let entry = map.get(slug)?;
    if entry.mtime == mtime {
        entry
            .last_access
            .store(clock.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
        Some(Arc::clone(&entry.idx))
    } else {
        // Stale (cron rewrote the file): treat as a miss. The caller reopens and
        // `insert` overwrites this entry, dropping the stale handle.
        None
    }
}

fn insert<V>(
    map: &DashMap<String, Entry<V>>,
    clock: &AtomicU64,
    capacity: usize,
    slug: &str,
    path: &Path,
    idx: FuzzyIndex<V>,
) -> Arc<FuzzyIndex<V>>
where
    V: DictionaryValue + Clone + Send + Sync + 'static,
{
    let arc = Arc::new(idx);
    let entry = Entry {
        idx: Arc::clone(&arc),
        mtime: file_mtime(path),
        last_access: AtomicU64::new(clock.fetch_add(1, Ordering::Relaxed)),
    };
    // Overwrites any stale entry; the replaced handle's daemon threads are
    // joined by `PersistentARTrieChar::Drop` once no in-flight reader holds it.
    map.insert(slug.to_string(), entry);
    evict_if_over_capacity(map, capacity);
    arc
}

fn evict_if_over_capacity<V>(map: &DashMap<String, Entry<V>>, capacity: usize)
where
    V: DictionaryValue + Clone + Send + Sync + 'static,
{
    while map.len() > capacity {
        // Choose the least-recently-accessed key, then remove it WITHOUT holding
        // an iteration ref across the remove (DashMap shard locks are not
        // re-entrant). The `RefMulti` is dropped at the end of the `let`.
        let victim = map
            .iter()
            .min_by_key(|e| e.last_access.load(Ordering::Relaxed))
            .map(|e| e.key().clone());
        match victim {
            Some(key) => {
                map.remove(&key);
            }
            None => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn touch(path: &Path) {
        std::fs::write(path, b"x").expect("write stamp file");
    }

    #[test]
    fn miss_then_reuse_then_mtime_invalidation() {
        let dir = tempdir().expect("tempdir");
        let cache = FuzzyCache::new();
        let path = dir.path().join("symbols.artrie");
        touch(&path);

        // Cold: nothing cached yet.
        assert!(cache.get_symbols("proj", &path).is_none());

        // Insert, then a get with the same mtime is a hit returning the same Arc.
        let idx = FuzzyIndex::<SymbolValue>::open_or_create(&dir.path().join("sym.trie"))
            .expect("open")
            .0;
        let a = cache.insert_symbols("proj", &path, idx);
        let b = cache.get_symbols("proj", &path).expect("hit");
        assert!(Arc::ptr_eq(&a, &b), "cache should return the same handle");

        // Bump the stamp file's mtime → the entry is now stale → miss.
        std::thread::sleep(std::time::Duration::from_millis(10));
        touch(&path);
        assert!(
            cache.get_symbols("proj", &path).is_none(),
            "an mtime change must invalidate the cached handle"
        );
    }

    #[test]
    fn capacity_is_bounded() {
        let dir = tempdir().expect("tempdir");
        let cache = FuzzyCache::with_capacity(2);
        for i in 0..5 {
            let stamp = dir.path().join(format!("p{i}.artrie"));
            touch(&stamp);
            let idx =
                FuzzyIndex::<SymbolValue>::open_or_create(&dir.path().join(format!("t{i}.trie")))
                    .expect("open")
                    .0;
            cache.insert_symbols(&format!("p{i}"), &stamp, idx);
        }
        assert!(
            cache.symbols.len() <= 2,
            "symbol cache must not exceed its capacity"
        );
    }
}
