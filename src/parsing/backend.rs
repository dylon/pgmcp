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
}
