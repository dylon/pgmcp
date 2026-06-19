//! Retrieval-quality evaluation harness for pgmcp's search tools.
//!
//! This is the campaign machinery behind `docs/evaluation/semantic-search-quality.md`:
//! it measures whether `semantic_search` / `hybrid_search` / `text_search`
//! actually rank relevant code highly, using labeled ground truth and the
//! rank-based metrics in [`pgmcp::quality::retrieval_metrics`].
//!
//! ## Modules
//!
//! - [`query`] — the [`query::EvalQuery`] / [`query::GoldTarget`] vocabulary +
//!   the hand-authored known-item query set (strategy **A**).
//! - [`docstring`] — pure, golden-tested extraction of a natural-language
//!   doc-comment from a code chunk (strategy **B** ground truth), plus
//!   identifier redaction for the M3 leakage-control variant.
//!
//! The DB-backed corpus selection (strategy **B** query generation + the M1
//! strip-and-re-embed leakage control), the tool runner, and the statistics +
//! experiment-ledger persistence live in the `eval_retrieval` campaign binary
//! (`pgmcp-testing/src/bin/eval_retrieval.rs`), which needs a live database and
//! embedder and so cannot be a pure unit-testable module.

pub mod corpus;
pub mod docstring;
pub mod query;
pub mod runner;
pub mod stats;

pub use docstring::{DocExtraction, extract_leading_docstring, redact_identifiers};
pub use query::{EvalQuery, GoldTarget, QuerySet, QueryStrategy};
pub use runner::SearchMode;
pub use stats::{AlignedMetric, PairwiseComparison, compare_all_pairs};
