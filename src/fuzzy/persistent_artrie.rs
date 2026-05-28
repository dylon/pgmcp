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
use libdictenstein::persistent_artrie_char::{PersistentARTrieChar, SharedCharARTrie};
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
/// Backed by `PersistentARTrieChar` wrapped in
/// `SharedCharARTrie<V> = Arc<RwLock<PersistentARTrieChar<V>>>`. Reads
/// take a parking_lot read guard (lock-free in the contention-free
/// case); writes take the write guard. Queries build a fresh
/// `Transducer` per call — the underlying `Arc<RwLock<...>>` clones
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
            let (trie, report) = PersistentARTrieChar::<V>::open_with_recovery(path)?;
            let storage = std::sync::Arc::new(parking_lot::RwLock::new(trie));
            Ok((
                Self {
                    storage,
                    path: path.to_path_buf(),
                },
                Some(report),
            ))
        } else {
            let trie = PersistentARTrieChar::<V>::create(path)?;
            let storage = std::sync::Arc::new(parking_lot::RwLock::new(trie));
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
        let mut guard = self.storage.write();
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
        let (idx, report) = FuzzyIndex::<()>::open_or_create(&path).expect("create");
        assert!(report.is_none(), "fresh trie should not run recovery");
        idx.upsert("hello", ()).unwrap();
        idx.upsert("world", ()).unwrap();
        assert!(idx.contains("hello"));
        assert!(idx.contains("world"));
        assert_eq!(idx.len(), 2);
        drop(idx);

        // Re-open: should recover prior entries.
        let (idx2, report2) = FuzzyIndex::<()>::open_or_create(&path).expect("reopen");
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
}
