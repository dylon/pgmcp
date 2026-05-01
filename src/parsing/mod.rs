//! Tree-sitter parsing layer (Tier 0e infrastructure).
//!
//! Per-language `LanguageBackend` impls extract three goals from a file:
//! 1. **Symbol definitions** — function / struct / enum / trait / interface /
//!    class / const / module declarations with name + line range.
//! 2. **Imports** — replaces `src/graph/import_extractor.rs` regex with a
//!    tree-sitter query, normalized to one canonical target form per language.
//! 3. **Symbol references** — call expressions, type usages, method invocations,
//!    captured with the referenced name and source line.
//!
//! The trait is intentionally minimal so each language backend lives in a
//! single file. Backends are added incrementally (`src/parsing/rust.rs`,
//! `src/parsing/python.rs`, …); the corresponding `tree-sitter-<lang>` crate
//! is added to `Cargo.toml` only when its backend lands. Until then,
//! `LanguageRegistry::for_language` returns `None` and callers fall back to
//! the existing regex paths.
//!
//! See `~/.claude/plans/help-me-design-software-ancient-flurry.md` Tier 0e
//! for the full plan, including the migration of `import_extractor.rs` and
//! the `symbol-extraction` cron job.

pub mod c_cpp;
pub mod clojure;
pub mod java;
pub mod javascript;
pub mod python;
pub mod rholang;
pub mod rust;
pub mod scala;
pub mod symbols;

// Public surface for the trait + future backends. The `unused_imports`
// allow is needed because backends land incrementally — until the first
// concrete `LanguageBackend` impl uses `SymbolKind` / `SymbolRefKind`,
// rustc would otherwise reject the re-export. The types ARE used by
// downstream callers via `crate::parsing::SymbolKind` once any backend lands.
#[allow(unused_imports)]
pub use symbols::{Import, Symbol, SymbolKind, SymbolRefKind, SymbolReference};

/// One language's tree-sitter-driven extraction.
///
/// Implementations are stateless wrappers around a tree-sitter `Parser` plus
/// pre-compiled `Query` objects for symbols / imports / references. Each
/// `extract_*` call parses fresh from `content` because we don't cache parsed
/// trees across files.
#[allow(dead_code)] // Trait is wired up incrementally; first impl arrives with `parsing/rust.rs`.
pub trait LanguageBackend: Send + Sync {
    /// Stable language name (matches `indexed_files.language`, e.g. "rust").
    fn language_name(&self) -> &'static str;

    /// Extract symbol definitions from `content`. Returned `Symbol` rows have
    /// `file_id = 0` placeholder; the caller fills it in before persisting.
    fn extract_symbols(&self, content: &str) -> Vec<Symbol>;

    /// Extract import statements. Each `Import.target_raw` is the canonical
    /// resolvable form for this language (Rust path, Python module, JS specifier).
    fn extract_imports(&self, content: &str) -> Vec<Import>;

    /// Extract symbol-reference edges. Returned `SymbolReference` rows have
    /// `source_file_id = 0`/`target_*_id = None` placeholders; the cron job
    /// resolves `target_symbol_id` by joining `target_raw` against
    /// `file_symbols.name` after symbol persistence.
    fn extract_references(&self, content: &str) -> Vec<SymbolReference>;
}

/// Registry: dispatches a language string to the matching backend, or `None`
/// when no backend has been wired yet. The cron job uses the `None` arm to
/// fall back to the regex-based `src/graph/import_extractor.rs`.
#[allow(dead_code)] // Used by the future symbol-extraction cron job.
pub struct LanguageRegistry;

#[allow(dead_code)]
impl LanguageRegistry {
    /// Resolve a language name (matching `indexed_files.language`) to its
    /// backend. Returns `None` for languages whose backend hasn't landed yet.
    pub fn for_language(language: &str) -> Option<&'static dyn LanguageBackend> {
        match language {
            "rust" => Some(&rust::RUST_BACKEND),
            "python" => Some(&python::PYTHON_BACKEND),
            "javascript" => Some(&javascript::JS_BACKEND),
            "typescript" => Some(&javascript::TS_BACKEND),
            "tsx" => Some(&javascript::TSX_BACKEND),
            "java" => Some(&java::JAVA_BACKEND),
            "scala" => Some(&scala::SCALA_BACKEND),
            "c" => Some(&c_cpp::C_BACKEND),
            "cpp" => Some(&c_cpp::CPP_BACKEND),
            "rholang" => Some(&rholang::RHOLANG_BACKEND),
            "clojure" => Some(&clojure::CLOJURE_BACKEND),
            "clojurescript" => Some(&clojure::CLOJURESCRIPT_BACKEND),
            _ => None,
        }
    }

    /// Whether *any* backend is available. Used by tools that emit a
    /// `health.symbols_present` envelope to differentiate "no backend
    /// implemented yet" from "backend exists but no symbols extracted".
    pub fn any_backend_available() -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_dispatches_landed_backends() {
        assert!(LanguageRegistry::for_language("rust").is_some());
        assert!(LanguageRegistry::for_language("python").is_some());
        assert!(LanguageRegistry::for_language("javascript").is_some());
        assert!(LanguageRegistry::for_language("typescript").is_some());
        assert!(LanguageRegistry::for_language("tsx").is_some());
        assert!(LanguageRegistry::for_language("java").is_some());
        assert!(LanguageRegistry::for_language("scala").is_some());
        assert!(LanguageRegistry::for_language("c").is_some());
        assert!(LanguageRegistry::for_language("cpp").is_some());
        assert!(LanguageRegistry::for_language("rholang").is_some());
        assert!(LanguageRegistry::for_language("clojure").is_some());
        assert!(LanguageRegistry::for_language("clojurescript").is_some());
        // Backends not yet landed:
        assert!(LanguageRegistry::for_language("unknown_lang").is_none());
        assert!(LanguageRegistry::any_backend_available());
    }

    #[test]
    fn rust_backend_returns_correct_language_name() {
        let backend = LanguageRegistry::for_language("rust").expect("rust backend");
        assert_eq!(backend.language_name(), "rust");
    }
}
