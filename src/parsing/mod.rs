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

pub mod backend;
pub mod c_cpp;
pub mod clojure;
pub mod java;
pub mod javascript;
pub mod python;
pub mod registry;
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
pub use backend::LanguageBackend;
#[allow(unused_imports)]
pub use registry::LanguageRegistry;
#[allow(unused_imports)]
pub use symbols::{Import, Symbol, SymbolKind, SymbolRefKind, SymbolReference};

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
