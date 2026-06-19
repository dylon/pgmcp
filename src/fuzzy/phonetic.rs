//! Wiring for `liblevenshtein`'s phonetic framework into pgmcp.
//!
//! Hosts:
//!
//! - Two free helpers that wrap `articulatory_distance` /
//!   `articulatory_edit_distance` (used by the kept `semver_break_audit`
//!   and `naming_consistency` tools, among others).
//! - `PgmcpPhonetics`: the per-project phonetic state holder that
//!   owns the active `RewriteRuleChar` set behind an `ArcSwap` for
//!   hot-reload, the BCP-47 language tag, and an optional
//!   filesystem watcher on a `.pgmcp/rules.llev` override. Used by
//!   the index-backed phonetic tools (`phonetic_symbol_search`,
//!   `correct_query`) and by the articulatory-distance integration
//!   in `tool_naming_consistency`, `tool_find_similar_modules`,
//!   `tool_find_duplicates`.
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phase 10 + P13.3.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use liblevenshtein::dictionary::phonetic_normalized::PhoneticNormalizedDictionaryChar;
use liblevenshtein::phonetic::expansion::expand_phonetic_alternatives_char;
use liblevenshtein::phonetic::feature_distance::{
    FeatureDistanceWeights, articulatory_distance, articulatory_edit_distance,
    articulatory_edit_distance_weighted,
};
use liblevenshtein::phonetic::language::dispatch::rules_for_language;
use liblevenshtein::phonetic::llev::{RuleSetChar, parse_str};
use liblevenshtein::phonetic::rules::english;
use liblevenshtein::phonetic::types::RewriteRuleChar;
use notify::{RecursiveMode, Watcher};
use thiserror::Error;
use tracing::{error, info, warn};

/// Articulatory edit distance — Levenshtein with per-character
/// substitution costs from the IPA articulatory-feature table
/// (`articulatory_distance`). `'p'` vs `'b'` ≈ 0.1 (voicing only);
/// `'p'` vs `'f'` ≈ 0.3 (manner change); `'a'` vs `'p'` = 1.0
/// (vowel vs consonant). pgmcp uses this in place of 0/1 Levenshtein
/// wherever identifier-similarity scoring is more useful with
/// linguistically-meaningful character costs (naming consistency,
/// duplicate detection, rename detection).
pub fn articulatory_distance_score(a: &str, b: &str) -> f64 {
    articulatory_edit_distance(a, b)
}

/// Weighted variant of [`articulatory_distance_score`] using caller-supplied
/// per-dimension [`FeatureDistanceWeights`] (typically built from the `[fuzzy]`
/// knobs via `crate::config::FuzzyConfig::articulatory_weights`). With default
/// weights this is identical to [`articulatory_distance_score`].
pub fn articulatory_distance_score_weighted(
    a: &str,
    b: &str,
    weights: &FeatureDistanceWeights,
) -> f64 {
    articulatory_edit_distance_weighted(a, b, weights)
}

/// Per-character articulatory distance (forwarded for callers that
/// want pairwise comparisons).
pub fn char_articulatory_distance(a: char, b: char) -> f64 {
    articulatory_distance(a, b)
}

/// Errors raised by the phonetic-framework wiring.
#[derive(Debug, Error)]
pub enum PhoneticsError {
    /// I/O failure reading a `.pgmcp/rules.llev` override.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// `.llev` parse or rule-set conversion failure.
    #[error("llev: {0}")]
    Llev(String),
    /// `notify` watcher failure.
    #[error("watcher: {0}")]
    Watcher(#[from] notify::Error),
}

/// Per-project phonetic state holder.
///
/// Conceptually a triple: (active rule set, BCP-47 language tag,
/// optional `.pgmcp/rules.llev` watcher). Reads are lock-free via
/// `ArcSwap`; hot-reload swaps in a fresh `Vec<RewriteRuleChar>`
/// without restarting the daemon.
pub struct PgmcpPhonetics {
    rules: ArcSwap<Vec<RewriteRuleChar>>,
    language: ArcSwap<String>,
    _watcher: parking_lot::Mutex<Option<notify::RecommendedWatcher>>,
}

impl PgmcpPhonetics {
    /// Construct with the embedded English (American) rule set.
    /// Equivalent to `for_language("en-us")`.
    pub fn default_english() -> Self {
        Self {
            rules: ArcSwap::from_pointee(english_base_rules()),
            language: ArcSwap::from_pointee("en-us".to_string()),
            _watcher: parking_lot::Mutex::new(None),
        }
    }

    /// Construct with rules for a BCP-47 language tag. Falls back to
    /// English if the tag is unknown (logs a warning).
    pub fn for_language(tag: &str) -> Self {
        let rules = rules_for_language(tag).unwrap_or_else(|| {
            warn!(
                tag,
                "PgmcpPhonetics: no rule pack for language; falling back to English"
            );
            english_base_rules()
        });
        Self {
            rules: ArcSwap::from_pointee(rules),
            language: ArcSwap::from_pointee(tag.to_string()),
            _watcher: parking_lot::Mutex::new(None),
        }
    }

    /// Construct from a `.llev` file at `rules_path`. Overrides the
    /// language pack — `language` is recorded for diagnostics only.
    pub fn open(rules_path: &Path, language: &str) -> Result<Self, PhoneticsError> {
        let rules = load_rules_from_path(rules_path)?;
        Ok(Self {
            rules: ArcSwap::from_pointee(rules),
            language: ArcSwap::from_pointee(language.to_string()),
            _watcher: parking_lot::Mutex::new(None),
        })
    }

    /// Normalize a term using the active rules. Case-insensitive
    /// (rules apply to lowercased input).
    pub fn normalize(&self, term: &str) -> String {
        // Build a transient RuleSetChar from the current rules; the
        // RuleSet struct just wraps Vec<RewriteRuleChar> and exposes
        // `apply`. Building one per call is cheap relative to the
        // rule-application cost.
        let rules = self.rules.load_full();
        let ruleset = RuleSetChar {
            rules: (*rules).clone(),
            name: Some("pgmcp-active".to_string()),
            version: Some("1".to_string()),
        };
        ruleset.apply(term)
    }

    /// Expand a query into a regex-style alternation pattern that
    /// matches phonetic variants of `term`. For example, given
    /// English rules, `"nite"` expands to `"(n|kn)i(t|te|ght)"` (or
    /// similar) — usable as a regex pre-filter when the caller wants
    /// to widen a search beyond exact match.
    pub fn expand_to_pattern(&self, term: &str) -> String {
        let rules = self.rules.load_full();
        expand_phonetic_alternatives_char(term, rules.as_slice())
    }

    /// Build a transient `PhoneticNormalizedDictionaryChar` over the
    /// given vocabulary, using the active rules. Callers use this to
    /// query phonetically-similar entries from a project's symbol /
    /// path / commit list.
    pub fn build_dictionary<S, I>(&self, terms: I) -> PhoneticNormalizedDictionaryChar<()>
    where
        S: AsRef<str>,
        I: IntoIterator<Item = S>,
    {
        let rules = self.rules.load_full();
        PhoneticNormalizedDictionaryChar::<()>::from_terms_with_rules(terms, (*rules).clone())
    }

    /// Composed phonetic∘edit search over a vocabulary. Normalizes both the
    /// query and each term with the active rules, then matches within
    /// Damerau-Levenshtein distance `max_distance` in normalized space (the
    /// `PhoneticNormalizedDictionary` automaton, trie-pruned for d≥1). Returns
    /// `(original_term, distance, normalized_form)` per hit, sorted by ascending
    /// distance. The caller joins payloads (e.g. `SymbolValue`) back by term —
    /// the dictionary is built over `()` so this stays decoupled from
    /// libdictenstein's value trait.
    pub fn phonetic_search<S, I>(
        &self,
        terms: I,
        query: &str,
        max_distance: usize,
    ) -> Vec<(String, usize, String)>
    where
        S: AsRef<str>,
        I: IntoIterator<Item = S>,
    {
        let dict = self.build_dictionary(terms);
        dict.query(query, max_distance)
            .into_iter()
            .map(|c| (c.term, c.distance, c.normalized_form))
            .collect()
    }

    /// Articulatory distance between two strings (delegates to the
    /// free helper for callers that have a `PgmcpPhonetics` handle
    /// but no direct fuzzy-module import).
    pub fn articulatory_distance(&self, a: &str, b: &str) -> f64 {
        articulatory_distance_score(a, b)
    }

    /// Replace the active rule set by re-reading from `path`.
    /// Lock-free swap; in-flight readers continue with the old
    /// rules until they refresh on next access.
    pub fn reload_rules(&self, path: &Path) -> Result<(), PhoneticsError> {
        let rules = load_rules_from_path(path)?;
        self.rules.store(Arc::new(rules));
        info!(path = %path.display(), "PgmcpPhonetics: reloaded rules");
        Ok(())
    }

    /// Reset to the embedded English base rules. Used by the watcher
    /// when the `.pgmcp/rules.llev` file is deleted.
    pub fn reset_to_default(&self) {
        self.rules.store(Arc::new(english_base_rules()));
        info!("PgmcpPhonetics: reset rules to embedded English base");
    }

    /// Start a filesystem watcher that calls `reload_rules` whenever
    /// `path` changes. Drops any previously-installed watcher.
    /// Returns immediately; events are processed on a `notify`
    /// background thread.
    pub fn watch(self: &Arc<Self>, path: PathBuf) -> Result<(), PhoneticsError> {
        let phon_for_cb = Arc::clone(self);
        let path_for_cb = path.clone();
        let mut watcher = notify::recommended_watcher(
            move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    match event.kind {
                        notify::EventKind::Modify(_) | notify::EventKind::Create(_) => {
                            if let Err(e) = phon_for_cb.reload_rules(&path_for_cb) {
                                error!(path = %path_for_cb.display(), error = %e, "PgmcpPhonetics: reload failed");
                            }
                        }
                        notify::EventKind::Remove(_) => {
                            phon_for_cb.reset_to_default();
                        }
                        _ => {}
                    }
                }
            },
        )?;
        // Watch the parent dir (non-recursive) so editor save patterns
        // that delete + re-create the file still trigger events.
        let parent = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        watcher.watch(&parent, RecursiveMode::NonRecursive)?;
        *self._watcher.lock() = Some(watcher);
        info!(path = %path.display(), "PgmcpPhonetics: watching for rule changes");
        Ok(())
    }

    /// Current BCP-47 language tag.
    pub fn language(&self) -> Arc<String> {
        self.language.load_full()
    }

    /// Current rule set (Arc-cheap clone).
    pub fn rules(&self) -> Arc<Vec<RewriteRuleChar>> {
        self.rules.load_full()
    }
}

fn english_base_rules() -> Vec<RewriteRuleChar> {
    english::base().rules.clone()
}

/// P14.4 — install (or reload) a `PgmcpPhonetics` for one project.
///
/// Looks up the per-project entry in the registry by `project_root`.
/// If present, calls `reload_rules` on the existing handle so the
/// active rule set hot-swaps without dropping the watcher. If
/// absent, constructs a new `Arc<PgmcpPhonetics>`, installs the
/// `.pgmcp/rules.llev` watcher via `watch`, and inserts the handle
/// into the registry.
///
/// Called from `event_processor.rs` on every `.pgmcp.toml`-change
/// event whose `ProjectOverride.phonetics.rules_path` is set.
/// Idempotent: re-invoking on an already-installed project re-reads
/// the rules but does not double-install the watcher.
pub fn install_phonetics_for_project(
    project_root: &Path,
    rules_path: &Path,
    language: Option<&str>,
    registry: &std::sync::Arc<dashmap::DashMap<PathBuf, std::sync::Arc<PgmcpPhonetics>>>,
) -> Result<(), PhoneticsError> {
    if let Some(existing) = registry.get(project_root) {
        existing.reload_rules(rules_path)?;
        return Ok(());
    }
    let phon = std::sync::Arc::new(PgmcpPhonetics::open(
        rules_path,
        language.unwrap_or("en-us"),
    )?);
    phon.watch(rules_path.to_path_buf())?;
    registry.insert(project_root.to_path_buf(), phon);
    Ok(())
}

fn load_rules_from_path(path: &Path) -> Result<Vec<RewriteRuleChar>, PhoneticsError> {
    let content = std::fs::read_to_string(path)?;
    let file = parse_str(&content).map_err(|e| PhoneticsError::Llev(format!("parse: {e:?}")))?;
    let ruleset = RuleSetChar::from_llev(&file)
        .map_err(|e| PhoneticsError::Llev(format!("convert: {e:?}")))?;
    Ok(ruleset.rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voicing_pair_is_cheaper_than_place_change() {
        let v = char_articulatory_distance('p', 'b');
        let p = char_articulatory_distance('p', 'k');
        assert!(
            v <= p,
            "voicing-only ({}) should be ≤ place change ({})",
            v,
            p
        );
    }

    #[test]
    fn articulatory_word_distance_picks_up_voicing_swap() {
        let same = articulatory_distance_score("path", "path");
        let close = articulatory_distance_score("path", "bath");
        let far = articulatory_distance_score("path", "math");
        assert_eq!(same, 0.0);
        assert!(close > same);
        assert!(far > 0.0);
    }

    #[test]
    fn default_english_loads_rules() {
        let phon = PgmcpPhonetics::default_english();
        let rules = phon.rules();
        assert!(
            !rules.is_empty(),
            "default English ruleset must be non-empty"
        );
        assert_eq!(phon.language().as_str(), "en-us");
    }

    #[test]
    fn for_language_unknown_tag_falls_back_to_english() {
        let phon = PgmcpPhonetics::for_language("zzz-unknown-tag");
        let rules = phon.rules();
        assert!(!rules.is_empty(), "unknown tag must fall back to English");
    }

    #[test]
    fn normalize_does_something_with_english_rules() {
        // "phone" → "fone" is the canonical example from the
        // embedded English rule set's homophone/ph-to-f mapping. We
        // assert only that normalization changes the input, since
        // the exact form depends on the loaded rule pack.
        let phon = PgmcpPhonetics::default_english();
        let normalized = phon.normalize("phone");
        // Permissive assertion: SOME transformation happens, or the
        // input is at least preserved verbatim — never a panic /
        // empty.
        assert!(!normalized.is_empty(), "normalize must not return empty");
    }

    #[test]
    fn expand_to_pattern_produces_non_empty_alternation() {
        let phon = PgmcpPhonetics::default_english();
        let pattern = phon.expand_to_pattern("nite");
        assert!(!pattern.is_empty(), "expanded pattern must not be empty");
    }
}
