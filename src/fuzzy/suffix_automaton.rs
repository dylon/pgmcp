//! Substring-search index backed by `libdictenstein::SuffixAutomatonChar`.
//!
//! Where the `FuzzyIndex` answers "which terms are within edit distance
//! k of this string?", the suffix automaton answers "which terms
//! contain this string as a substring?" and "where in each term does
//! the match start?". Used by the `substring_search` and `fuzzy_grep`
//! MCP tools (Phase 8).

use libdictenstein::Dictionary;
use libdictenstein::suffix_automaton_char::SuffixAutomatonChar;

/// In-memory substring index.
pub struct SubstringIndex {
    storage: SuffixAutomatonChar<()>,
}

impl SubstringIndex {
    /// Build an empty index.
    pub fn empty() -> Self {
        Self {
            storage: SuffixAutomatonChar::new(),
        }
    }

    /// Build an index from an iterator of terms.
    pub fn from_terms<I, S>(terms: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            storage: SuffixAutomatonChar::from_texts(terms),
        }
    }

    /// Add a single term to the index. Returns `true` if the term was
    /// newly inserted, `false` if already present.
    pub fn add(&self, term: &str) -> bool {
        self.storage.insert(term)
    }

    /// Return `true` if any indexed term contains `pattern` as a
    /// substring.
    pub fn contains_substring(&self, pattern: &str) -> bool {
        self.storage.contains(pattern)
    }

    /// Number of indexed terms.
    pub fn len(&self) -> usize {
        self.storage.len().unwrap_or(0)
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for SubstringIndex {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_substring_picks_up_inner_match() {
        let idx = SubstringIndex::from_terms(["alphabet", "betacarotene", "gamma"]);
        assert!(idx.contains_substring("alpha"));
        assert!(idx.contains_substring("bet"));
        assert!(!idx.contains_substring("zzz"));
    }
}
