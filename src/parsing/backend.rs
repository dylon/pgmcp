use super::dataflow::FunctionDataflow;
use super::function_metrics::FunctionMetrics;
use super::symbols::{Import, Symbol, SymbolReference};

/// One language's tree-sitter-driven extraction.
///
/// Implementations are stateless wrappers around a tree-sitter `Parser` plus
/// pre-compiled `Query` objects for symbols, imports, and references. Each
/// `extract_*` call parses fresh from `content` because parsed trees are not
/// cached across files.
#[allow(dead_code)]
pub trait LanguageBackend: Send + Sync {
    /// Stable language name (matches `indexed_files.language`, e.g. "rust").
    fn language_name(&self) -> &'static str;

    /// Extract symbol definitions from `content`. Returned `Symbol` rows have
    /// `file_id = 0` placeholder; the caller fills it in before persisting.
    fn extract_symbols(&self, content: &str) -> Vec<Symbol>;

    /// Extract import statements. Each `Import.target_raw` is the canonical
    /// resolvable form for this language.
    fn extract_imports(&self, content: &str) -> Vec<Import>;

    /// Extract symbol-reference edges. Returned `SymbolReference` rows have
    /// source/target placeholders; the cron job resolves targets after symbol
    /// persistence.
    fn extract_references(&self, content: &str) -> Vec<SymbolReference>;

    /// Extract per-function complexity metrics (cyclomatic, cognitive,
    /// Halstead, NPath, panic-paths, unsafe-blocks). Returned rows have
    /// `function_id = 0` / `file_id = 0` placeholders; the function-metrics
    /// cron resolves them via `file_symbols` lookup keyed on
    /// `(file_id, kind='function', name, start_line)`.
    ///
    /// Default implementation returns `Vec::new()` so backends roll out
    /// incrementally — until a language's CFG/operator-vocabulary pass
    /// lands, that language simply has no per-function metrics.
    fn extract_function_metrics(&self, _content: &str) -> Vec<FunctionMetrics> {
        Vec::new()
    }

    /// Extract per-function intraprocedural data-flow facts (def-use edges +
    /// taint source/sink/sanitizer tags) for the taint engine
    /// (`crate::code_analysis::taint_dataflow`).
    ///
    /// Default returns `Vec::new()` so backends roll out incrementally (exactly
    /// like `extract_function_metrics`); until a language's def-use pass lands,
    /// `tool_taint_analysis` falls back to its regex co-occurrence heuristic for
    /// that language. Rust ships first (richest AST via `syn`).
    fn extract_dataflow(&self, _content: &str) -> Vec<FunctionDataflow> {
        Vec::new()
    }

    /// Extract the ordered intra-function synchronization skeleton (lock
    /// acquire/release, channel send/recv, spawn/await/select) for static
    /// deadlock + bottleneck analysis (`sync_ops`, migration `v21_sync_ops`).
    /// Returned `FunctionSyncOps` use the `(function, start_line)` identity the
    /// symbol-extraction cron keys to `file_symbols`.
    ///
    /// Default returns `Vec::new()` so backends roll out incrementally (exactly
    /// like `extract_dataflow`); a language with no sync-op pass keeps only its
    /// coarse `symbol_effects` membership and the analyzers' regex fallback.
    /// Rust + Rholang ship in v1.
    fn extract_sync_ops(&self, _content: &str) -> Vec<super::sync_ops::FunctionSyncOps> {
        Vec::new()
    }

    /// Per-language lexical config for textual occurrence extraction (ADR-024).
    /// Defaults to C-style (`//`, `/* */`, `"…"`); languages with other comment
    /// or string syntax override this (Python `#`, Lisp `;`, ML `(* *)`, …).
    fn lex_config(&self) -> super::occurrences::LexConfig {
        super::occurrences::LexConfig::c_style()
    }

    /// Extract every identifier occurrence (token-level) for `symbol_occurrences`
    /// (v45) — code references plus identifiers in comments / strings / doc
    /// comments, each with line + column offsets. The default drives the uniform
    /// lexical scanner with `lex_config()`, so EVERY backend produces occurrences
    /// (the extraction cron marks definitions and attaches binder `type_tags`).
    /// A backend may override for grammar-precise extraction.
    fn extract_occurrences(&self, content: &str) -> Vec<super::symbols::Occurrence> {
        super::occurrences::extract_occurrences_textual(content, &self.lex_config())
    }
}
