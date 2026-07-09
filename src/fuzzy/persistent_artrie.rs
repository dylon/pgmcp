//! `FuzzyIndex<V>` — the disk-backed PersistentARTrieChar wrapper that
//! powers pgmcp's fuzzy-search MCP tools.
//!
//! PG remains the source of truth; the trie is a hot-path index that
//! survives daemon restarts via mmap + WAL ACID semantics
//! (libdictenstein's PersistentARTrieChar, with TLA+-verified
//! recovery). On daemon start the trie is opened with
//! `open_with_recovery`; corruption triggers a synchronous rebuild
//! from PG before the daemon serves requests.

use std::path::{Path, PathBuf};

use libdictenstein::Dictionary;
use libdictenstein::DictionaryValue;
use libdictenstein::EvictableARTrie;
use libdictenstein::MappedDictionary;
use libdictenstein::MutableMappedDictionary;
use libdictenstein::persistent_artrie::eviction::{EvictionConfig, EvictionStats};
use libdictenstein::persistent_artrie::recovery::RecoveryReport;
// `read()`/`write()` on the shared trie handle now come from this trait (the
// libdictenstein overlay refactor moved concurrency inside the trie — both are
// lock-free shared borrows; see the doc on `SharedTrieAccess`).
use libdictenstein::persistent_artrie::SharedTrieAccess;
use libdictenstein::persistent_artrie::char::{PersistentARTrieChar, SharedCharARTrie};
use liblevenshtein::transducer::{Algorithm, Transducer};

/// Errors surfaced by the FuzzyIndex layer.
#[derive(Debug, thiserror::Error)]
pub enum FuzzyError {
    /// libdictenstein persistent-artrie error.
    #[error("persistent trie error: {0}")]
    Trie(String),
}

impl From<libdictenstein::persistent_artrie::error::PersistentARTrieError> for FuzzyError {
    fn from(e: libdictenstein::persistent_artrie::error::PersistentARTrieError) -> Self {
        FuzzyError::Trie(e.to_string())
    }
}

/// A disk-backed fuzzy index over `(term, V)` pairs.
///
/// Backed by `SharedCharARTrie<V> = Arc<PersistentARTrieChar<V>>` — the
/// libdictenstein overlay refactor moved concurrency *inside* the trie
/// (a lock-free overlay heap), so there is no external `RwLock`. The
/// `read()`/`write()` accessors (from the `SharedTrieAccess` trait) are
/// now lock-free shared borrows; the `&self` mutators route to lock-free
/// CAS. Queries build a fresh `Transducer` per call — the `Arc` clones
/// cheaply, and the transducer is a tiny wrapper around the dictionary
/// + algorithm choice.
pub struct FuzzyIndex<V>
where
    V: DictionaryValue + Clone + Send + Sync + 'static,
{
    storage: SharedCharARTrie<V>,
    path: PathBuf,
}

impl<V> FuzzyIndex<V>
where
    V: DictionaryValue + Clone + Send + Sync + 'static,
{
    /// Open an existing trie or create one if the path does not exist.
    /// Returns the index plus a [`RecoveryReport`] describing what (if
    /// anything) WAL recovery had to do at open time.
    pub fn open_or_create(path: &Path) -> Result<(Self, Option<RecoveryReport>), FuzzyError> {
        std::fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new(".")))
            .map_err(|e| FuzzyError::Trie(format!("mkdir: {e}")))?;

        // Sniff for an existing trie file. PersistentARTrieChar uses the
        // path itself as the primary file; if it doesn't exist, create.
        if path.exists() {
            // The Tier-1 single-owner advisory lock is released when the prior
            // handle's trie is dropped — but not always *synchronously*: a
            // background overlay worker can hold the lock for a few ms after the
            // `Arc` refcount hits zero, and a fast restart after an OOM-kill
            // (systemd `RestartSec=5`) can beat the dying process's flock release.
            // Retry a bounded number of times on a transient `FileLocked` to ride
            // out that window; any other error, or exhausting the retries,
            // propagates. (Root cause is libdictenstein's async drop-release; this
            // is the resilient open at pgmcp's layer.)
            use libdictenstein::persistent_artrie::error::PersistentARTrieError;
            const MAX_ATTEMPTS: u32 = 10;
            const BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);
            let mut attempt: u32 = 0;
            let (trie, report) = loop {
                match PersistentARTrieChar::<V>::open_with_recovery(path) {
                    Ok(ok) => break ok,
                    Err(PersistentARTrieError::FileLocked { .. }) if attempt + 1 < MAX_ATTEMPTS => {
                        attempt += 1;
                        std::thread::sleep(BACKOFF);
                    }
                    Err(e) => return Err(e.into()),
                }
            };
            let storage = std::sync::Arc::new(trie);
            Ok((
                Self {
                    storage,
                    path: path.to_path_buf(),
                },
                Some(report),
            ))
        } else {
            let trie = PersistentARTrieChar::<V>::create(path)?;
            let storage = std::sync::Arc::new(trie);
            Ok((
                Self {
                    storage,
                    path: path.to_path_buf(),
                },
                None,
            ))
        }
    }

    /// Filesystem path of this trie.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Insert / overwrite a term + value pair.
    pub fn upsert(&self, term: &str, value: V) -> Result<(), FuzzyError> {
        self.storage.insert_with_value(term, value);
        Ok(())
    }

    /// Remove a term. Returns `true` if the term was present.
    pub fn remove(&self, term: &str) -> Result<bool, FuzzyError> {
        let guard = self.storage.write();
        guard.remove(term).map_err(Into::into)
    }

    /// Exact-membership check.
    pub fn contains(&self, term: &str) -> bool {
        self.storage.contains(term)
    }

    /// Get the value associated with a term, if present.
    pub fn get(&self, term: &str) -> Option<V> {
        self.storage.get_value(term)
    }

    /// Number of terms in the index.
    pub fn len(&self) -> usize {
        self.storage.len().unwrap_or(0)
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Approximate query: returns `(term, distance, value)` tuples for
    /// terms within Damerau-Levenshtein distance `max_distance` of `query`.
    /// Results are de-duplicated by term; multiple values per term are
    /// not represented in this trie (use a multi-trie composition).
    pub fn query(&self, query: &str, max_distance: usize) -> Vec<(String, usize, V)> {
        let storage_for_transducer = self.storage.clone();
        let transducer = Transducer::new(storage_for_transducer, Algorithm::Transposition);
        // `query_values` reads each match's value during the automaton traversal
        // (a single walk) — no per-candidate `get` re-walk. Enabled by the
        // `MappedDictionaryNode` impl on `PersistentARTrieCharNode`.
        transducer.query_values(query, max_distance).collect()
    }

    /// Approximate query with a value-side predicate filter applied at
    /// candidate enumeration time. Useful for type-tag / effect filters
    /// surfaced through MCP tool params.
    pub fn query_filtered<F>(
        &self,
        query: &str,
        max_distance: usize,
        predicate: F,
    ) -> Vec<(String, usize, V)>
    where
        F: Fn(&V) -> bool,
    {
        // Single-walk value-yielding query with the value predicate applied to
        // each match — e.g. fuzzy symbol search restricted to public symbols or
        // a given kind, without a second lookup per candidate.
        let storage_for_transducer = self.storage.clone();
        let transducer = Transducer::new(storage_for_transducer, Algorithm::Transposition);
        transducer
            .query_values(query, max_distance)
            .filter(|(_, _, value)| predicate(value))
            .collect()
    }

    /// Prefix (autocomplete) search: every stored term beginning with `prefix`,
    /// paired with its value, in the trie's enumeration order. Linear in the
    /// prefix length via the trie's `iter_prefix_with_values` — the right
    /// primitive for resource-template completion and prefix-narrowed lookup.
    /// `limit == 0` means uncapped. A missing prefix path or a read error yields
    /// an empty vec (fails closed, like [`contains`](Self::contains)). Collected
    /// under the read guard and returned owned, so no lock is held across an await.
    pub fn prefix(&self, prefix: &str, limit: usize) -> Vec<(String, V)> {
        match self.storage.read().iter_prefix_with_values(prefix) {
            Ok(Some(mut hits)) => {
                if limit > 0 && hits.len() > limit {
                    hits.truncate(limit);
                }
                hits
            }
            Ok(None) | Err(_) => Vec::new(),
        }
    }

    /// Collect every term in the trie as owned `String`s. Collected under
    /// the read guard and returned owned, so no lock is held across an await.
    pub fn iter_strings(&self) -> Vec<String> {
        self.storage.read().iter().collect()
    }

    /// Collect every `(term, value)` pair in the trie. Used by composed
    /// search (e.g. building a transient phonetic-normalized dictionary over
    /// the project vocabulary and joining values back by term).
    pub fn iter_with_values(&self) -> Vec<(String, V)> {
        self.storage.read().iter_with_values().collect()
    }

    /// Returns the underlying shared-trie handle, for callers that
    /// need to share the storage across components (e.g. a sync cron
    /// that writes while a tool reads).
    pub fn storage(&self) -> SharedCharARTrie<V> {
        self.storage.clone()
    }

    /// Enable heap eviction: under system memory pressure the libdictenstein
    /// eviction coordinator reclaims in-memory node boxes (swizzling them to
    /// their on-disk locations, which `checkpoint` records). Idempotent guard
    /// inside libdictenstein returns an error if already enabled.
    pub fn enable_eviction(&self, config: EvictionConfig) -> Result<(), FuzzyError> {
        self.storage
            .enable_eviction(config)
            .map_err(|e| FuzzyError::Trie(format!("enable_eviction: {e}")))
    }

    /// Whether heap eviction is currently enabled on this trie.
    pub fn eviction_enabled(&self) -> bool {
        self.storage.eviction_enabled()
    }

    /// Cumulative eviction statistics for this trie instance.
    pub fn eviction_stats(&self) -> EvictionStats {
        self.storage.eviction_stats()
    }

    /// Force eviction of at least `target_bytes` of in-memory node boxes,
    /// returning `(nodes_evicted, bytes_freed)`. Only effective once a
    /// `checkpoint` has populated the disk-location registry.
    pub fn force_eviction(&self, target_bytes: usize) -> Result<(usize, usize), FuzzyError> {
        self.storage
            .force_eviction(target_bytes)
            .map_err(|e| FuzzyError::Trie(format!("force_eviction: {e}")))
    }

    /// Checkpoint the trie to disk. Persists pending mutations and, when
    /// eviction is enabled, (re)populates the eviction coordinator's
    /// disk-location registry so eviction can reclaim node boxes.
    pub fn checkpoint(&self) -> Result<(), FuzzyError> {
        self.storage
            .write()
            .checkpoint()
            .map_err(|e| FuzzyError::Trie(format!("checkpoint: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn open_or_create_creates_then_reopens() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("test.artrie");

        // First open: create a new trie.
        let (idx, report) = FuzzyIndex::<i64>::open_or_create(&path).expect("create");
        assert!(report.is_none(), "fresh trie should not run recovery");
        idx.upsert("hello", 1).unwrap();
        idx.upsert("world", 2).unwrap();
        assert!(idx.contains("hello"));
        assert!(idx.contains("world"));
        assert_eq!(idx.len(), 2);
        drop(idx);

        // Re-open: should recover prior entries.
        let (idx2, report2) = FuzzyIndex::<i64>::open_or_create(&path).expect("reopen");
        assert!(report2.is_some(), "reopen should produce a recovery report");
        assert!(idx2.contains("hello"));
        assert!(idx2.contains("world"));
        assert_eq!(idx2.len(), 2);
    }

    #[test]
    fn query_returns_within_max_distance() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("query.artrie");
        let (idx, _) = FuzzyIndex::<()>::open_or_create(&path).expect("create");
        idx.upsert("receive", ()).unwrap();
        idx.upsert("decide", ()).unwrap();
        idx.upsert("recipe", ()).unwrap();

        // `recieve` is one transposition from `receive`.
        let hits = idx.query("recieve", 2);
        assert!(hits.iter().any(|(t, _, _)| t == "receive"));
    }

    #[test]
    fn prefix_returns_terms_with_values() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("prefix.artrie");
        let (idx, _) = FuzzyIndex::<i64>::open_or_create(&path).expect("create");
        idx.upsert("concurrency", 1).unwrap();
        idx.upsert("concurrent", 2).unwrap();
        idx.upsert("consensus", 3).unwrap();
        idx.upsert("deadlock", 4).unwrap();

        // The "concur" prefix returns both concurrency terms (with values) and
        // neither "consensus" nor "deadlock".
        let hits = idx.prefix("concur", 0);
        let names: Vec<&str> = hits.iter().map(|(t, _)| t.as_str()).collect();
        assert!(names.contains(&"concurrency"));
        assert!(names.contains(&"concurrent"));
        assert!(!names.contains(&"consensus"));
        assert!(!names.contains(&"deadlock"));
        assert!(hits.iter().any(|(t, v)| t == "concurrency" && *v == 1));

        // `limit` caps the result count; a non-matching prefix fails closed to empty.
        assert_eq!(idx.prefix("concur", 1).len(), 1);
        assert!(idx.prefix("zzz", 0).is_empty());
    }

    #[test]
    fn query_descends_after_reopen() {
        // Mirrors the daemon: the `fuzzy-sync` cron writes + checkpoints the trie
        // in one process, and a later tool process reopens it (mmap-attach, with
        // children swizzled to disk) and queries. Before the libdictenstein
        // swizzle-aware DictionaryNode fix, `FuzzyIndex::query` returned ZERO hits
        // here even for an exact match — `len()`/`iter_strings()` were correct but
        // the transducer could not descend past the resident root. This is the
        // exact production failure observed via `fuzzy_symbol_search`.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("reopen.artrie");
        {
            let (idx, _) = FuzzyIndex::<i64>::open_or_create(&path).expect("create");
            idx.upsert("receive", 10).unwrap();
            idx.upsert("decide", 20).unwrap();
            idx.upsert("recipe", 30).unwrap();
            idx.checkpoint().expect("checkpoint");
        } // drop -> in-memory node boxes released; only the on-disk image remains

        let (idx2, report) = FuzzyIndex::<i64>::open_or_create(&path).expect("reopen");
        assert!(report.is_some(), "reopen should run recovery");
        assert_eq!(idx2.len(), 3);

        // Exact (distance 0) and fuzzy (one transposition) must both descend the
        // reopened persistent trie; the value must survive through the faulted
        // node (`MappedDictionaryNode::value`).
        let exact = idx2.query("receive", 0);
        assert!(
            exact.iter().any(|(t, _, v)| t == "receive" && *v == 10),
            "exact query lost after reopen: {exact:?}"
        );
        let fuzzy = idx2.query("recieve", 2);
        assert!(
            fuzzy.iter().any(|(t, _, _)| t == "receive"),
            "fuzzy query lost after reopen: {fuzzy:?}"
        );
    }

    /// The 2026-07-08 fuzzy-sync OOM fix hinges on the resident-budget eviction
    /// tail ACTUALLY reclaiming char-trie overlay nodes on checkpoint. The byte
    /// `force_eviction` path is a no-op for a char trie, so the fix relies on the
    /// char `force_eviction_char_resident` path, which fires only when
    /// `resident_budget_bytes` is `Some`. This bulk-loads far more than a tiny
    /// budget with periodic checkpoints (the `rebuild_*` pattern) and asserts
    /// (a) eviction actually reclaimed nodes — else the fix is inert — and
    /// (b) every term survives eviction + reopen (RAM is bounded WITHOUT sacrificing
    /// completeness). This is the gate the OOM fix is built on.
    // The gate proving the char resident-budget eviction reclaims nodes AND
    // preserves all terms across incremental checkpoints. It was `#[ignore]`d while
    // the libdictenstein char-eviction corruption (arena/block off-by-one in
    // check_sequential_char_children) was open; that fix landed 2026-07-08, so this
    // now RUNS and must pass — it's the green gate for `[fuzzy] resident_budget_bytes`
    // being active by default.
    #[test]
    fn resident_budget_eviction_reclaims_and_preserves_terms() {
        use libdictenstein::persistent_artrie::eviction::EvictionConfig;
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evict.artrie");
        let (idx, _) = FuzzyIndex::<i64>::open_or_create(&path).expect("create");
        // Tiny resident budget forces the post-checkpoint eviction tail to swizzle
        // cold overlay nodes to disk during the bulk load. Disable the async
        // memory-pressure monitor so the test drives eviction purely via checkpoints.
        idx.enable_eviction(EvictionConfig {
            resident_budget_bytes: Some(64 * 1024),
            enable_memory_pressure_monitor: false,
            ..EvictionConfig::default()
        })
        .expect("enable eviction");

        const N: i64 = 20_000;
        for i in 0..N {
            idx.upsert(&format!("symbol_{i:08}"), i).expect("upsert");
            if i % 2_000 == 0 {
                idx.checkpoint().expect("checkpoint");
            }
        }
        idx.checkpoint().expect("final checkpoint");

        // (a) The char resident-budget eviction path actually reclaimed nodes. If
        // this is 0 the char eviction is a no-op and the OOM fix does nothing.
        let ev = idx.eviction_stats();
        assert!(
            ev.nodes_evicted > 0,
            "resident-budget eviction must reclaim char overlay nodes during a bulk load \
             (nodes_evicted={}, bytes_freed={})",
            ev.nodes_evicted,
            ev.bytes_freed
        );

        // (b) Completeness: eviction swizzles cold nodes to disk — they are NOT
        // lost. `len()` mid-build reflects only the RESIDENT set (evicted nodes live
        // on disk until faulted), so completeness is verified after drop + reopen —
        // reopen eager-loads the full image, so every term must be recoverable.
        drop(idx);
        let (idx2, _) = FuzzyIndex::<i64>::open_or_create(&path).expect("reopen");
        assert_eq!(
            idx2.len(),
            N as usize,
            "all terms survive eviction + reopen"
        );
        for i in (0..N).step_by(2_500) {
            let term = format!("symbol_{i:08}");
            assert!(
                idx2.contains(&term),
                "term {term} survives eviction + reopen"
            );
        }
    }
}
