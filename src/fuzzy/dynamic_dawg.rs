//! In-memory DynamicDawgChar-backed fuzzy index for short-lived
//! session-scoped state (mandate dedup, query-time vocabularies).
//!
//! Volatile-by-design: nothing is persisted to disk. The on-disk
//! durable analog is [`crate::fuzzy::FuzzyIndex`].

use libdictenstein::Dictionary;
use libdictenstein::DictionaryValue;
use libdictenstein::dynamic_dawg::char::DynamicDawgChar;
use liblevenshtein::transducer::{Algorithm, Transducer};

// `MappedDictionary` / `MutableMappedDictionary` traits are pulled in via
// glob below so the inherent-vs-trait method dispatch (`get_value`,
// `insert_with_value`) resolves regardless of whether the caller
// imports the traits themselves.
#[allow(unused_imports)]
use libdictenstein::{MappedDictionary, MutableMappedDictionary};

/// In-memory, lock-internal, thread-safe fuzzy index over a
/// `DynamicDawgChar<V>`.
pub struct InMemoryFuzzyIndex<V>
where
    V: DictionaryValue + Clone + Send + Sync + 'static,
{
    storage: DynamicDawgChar<V>,
}

impl<V> InMemoryFuzzyIndex<V>
where
    V: DictionaryValue + Clone + Send + Sync + 'static,
{
    /// Construct an empty index.
    pub fn empty() -> Self {
        Self {
            storage: DynamicDawgChar::new(),
        }
    }

    /// Build an index from an iterator of `(term, value)` pairs.
    pub fn from_pairs<I, S>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (S, V)>,
        S: AsRef<str>,
    {
        let storage = DynamicDawgChar::new();
        for (term, value) in pairs {
            storage.insert_with_value(term.as_ref(), value);
        }
        Self { storage }
    }

    /// Insert / overwrite a `(term, value)` pair.
    pub fn upsert(&self, term: &str, value: V) {
        self.storage.insert_with_value(term, value);
    }

    /// Exact membership check.
    pub fn contains(&self, term: &str) -> bool {
        self.storage.contains(term)
    }

    /// Fetch the value bound to `term`, if present.
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
    pub fn query(&self, query: &str, max_distance: usize) -> Vec<(String, usize, V)> {
        let storage_for_transducer = self.storage.clone();
        let transducer = Transducer::new(storage_for_transducer, Algorithm::Transposition);
        let mut out = Vec::new();
        for candidate in transducer.query_with_distance(query, max_distance) {
            if let Some(value) = self.get(&candidate.term) {
                out.push((candidate.term, candidate.distance, value));
            }
        }
        out
    }
}

impl<V> Default for InMemoryFuzzyIndex<V>
where
    V: DictionaryValue + Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_pairs_then_query() {
        let idx: InMemoryFuzzyIndex<i64> =
            InMemoryFuzzyIndex::from_pairs([("alpha", 1i64), ("beta", 2i64), ("gamma", 3i64)]);
        assert_eq!(idx.len(), 3);
        let hits = idx.query("alpha", 0);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "alpha");
        assert_eq!(hits[0].1, 0);
        assert_eq!(hits[0].2, 1);
    }

    #[test]
    fn dedupes_within_max_distance() {
        let idx: InMemoryFuzzyIndex<i64> = InMemoryFuzzyIndex::from_pairs([
            ("use unwrap", 7i64),
            ("use unwraps", 8i64),
            ("unrelated", 9i64),
        ]);
        let hits = idx.query("use unwrap", 2);
        let terms: Vec<&String> = hits.iter().map(|(t, _, _)| t).collect();
        assert!(terms.iter().any(|t| t.as_str() == "use unwrap"));
        assert!(terms.iter().any(|t| t.as_str() == "use unwraps"));
        assert!(!terms.iter().any(|t| t.as_str() == "unrelated"));
    }
}
