//! Approximate-string-matching backends for pgmcp.
//!
//! Built on the sibling crates `libdictenstein` (dictionary backends
//! including `PersistentARTrieChar` disk-backed mmap+WAL trie) and
//! `liblevenshtein` (Levenshtein automata + phonetic framework).
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`.

pub mod cache;
pub mod disk_guard;
pub mod dynamic_dawg;
pub mod limits;
pub mod persistent_artrie;
pub mod phonetic;
pub mod suffix_automaton;
pub mod sync;
pub mod time_series;
pub mod trajectory_index;
pub mod values;

#[allow(unused_imports)]
pub use dynamic_dawg::InMemoryFuzzyIndex;
#[allow(unused_imports)]
pub use persistent_artrie::{FuzzyError, FuzzyIndex};
#[allow(unused_imports)]
pub use values::{CommitRef, DurableMandateRef, PathValue, SymbolValue};
