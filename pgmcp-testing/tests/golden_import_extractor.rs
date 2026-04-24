//! Golden-file tests for `pgmcp::graph::import_extractor`.
//!
//! One fixture per supported language (Rust, Python, JavaScript,
//! Java, Go, C). Catches regressions in the regex definitions or
//! the language-dispatch logic.

use pgmcp::graph::import_extractor::{self, RawImport};
use pgmcp_testing::golden::assert_match_exact;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImportInput {
    content: String,
    language: String,
}

fn run_extract(input: &ImportInput) -> Vec<RawImport> {
    import_extractor::extract_imports(&input.content, &input.language)
}

#[test]
fn rust_use_mod_extern_matches_golden() {
    assert_match_exact::<ImportInput, Vec<RawImport>>(
        "import_extractor/rust_use_mod_extern",
        run_extract,
    );
}

#[test]
fn python_import_from_matches_golden() {
    assert_match_exact::<ImportInput, Vec<RawImport>>(
        "import_extractor/python_import_from",
        run_extract,
    );
}

#[test]
fn javascript_import_require_matches_golden() {
    assert_match_exact::<ImportInput, Vec<RawImport>>(
        "import_extractor/javascript_import_require",
        run_extract,
    );
}

#[test]
fn java_import_matches_golden() {
    assert_match_exact::<ImportInput, Vec<RawImport>>("import_extractor/java_import", run_extract);
}

#[test]
fn go_import_matches_golden() {
    assert_match_exact::<ImportInput, Vec<RawImport>>("import_extractor/go_import", run_extract);
}

#[test]
fn c_include_matches_golden() {
    assert_match_exact::<ImportInput, Vec<RawImport>>("import_extractor/c_include", run_extract);
}
