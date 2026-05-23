//! Approximate-string-matching backends for pgmcp.
//!
//! Built on the sibling crates `libdictenstein` (dictionary backends
//! including `PersistentARTrieChar` disk-backed mmap+WAL trie) and
//! `liblevenshtein` (Levenshtein automata + phonetic framework).
//!
//! Module population is staged across the integration plan; this top-level
//! file declares the namespace so subsequent phases can populate it without
//! re-touching `src/lib.rs`.
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phases that populate this module:
//! - **Phase 3**: drop-in `Transducer` replacements for the two existing
//!   `strsim` / `levenshtein_less_equal` call sites (no files added here).
//! - **Phase 4**: `persistent_artrie`, `dynamic_dawg`, `suffix_automaton`,
//!   `time_series`, `sync` submodules — disk-backed `FuzzyIndex<V>` and
//!   the PG-to-trie sync logic.
//! - **Phase 10**: `phonetic` submodule — the full liblevenshtein
//!   phonetic framework wiring (rule-set hot reload, articulatory
//!   distance callsites, `.pgmcp/rules.llev` per-project overrides).
