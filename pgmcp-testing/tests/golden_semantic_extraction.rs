//! Golden-file tests for shadow-ASR semantic extraction across all
//! supported backends. Each fixture pairs a canonical source snippet
//! with the expected `Symbol` vector — verifying parameter shape,
//! return type tags, effects, scope_path, etc.
//!
//! Run via `cargo test -p pgmcp-testing --test golden_semantic_extraction`.
//! Regenerate fixtures via `cargo run --release -p pgmcp-testing --bin regen-goldens`.

use pgmcp::parsing::registry::LanguageRegistry;
use pgmcp::parsing::symbols::Symbol;
use pgmcp_testing::golden::assert_match_exact;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SemanticExtractionInput {
    content: String,
    language: String,
}

fn run_extract(input: &SemanticExtractionInput) -> Vec<Symbol> {
    let Some(backend) = LanguageRegistry::for_language(&input.language) else {
        return Vec::new();
    };
    backend.extract_symbols(&input.content)
}

#[test]
fn rust_basic_matches_golden() {
    assert_match_exact::<SemanticExtractionInput, Vec<Symbol>>(
        "semantic_extraction/rust_basic",
        run_extract,
    );
}

#[test]
fn python_basic_matches_golden() {
    assert_match_exact::<SemanticExtractionInput, Vec<Symbol>>(
        "semantic_extraction/python_basic",
        run_extract,
    );
}

#[test]
fn metta_typed_rule_matches_golden() {
    assert_match_exact::<SemanticExtractionInput, Vec<Symbol>>(
        "semantic_extraction/metta_typed_rule",
        run_extract,
    );
}

#[test]
fn rholang_contract_matches_golden() {
    assert_match_exact::<SemanticExtractionInput, Vec<Symbol>>(
        "semantic_extraction/rholang_contract",
        run_extract,
    );
}
