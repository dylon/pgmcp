//! Value types stored alongside terms in each `FuzzyIndex`.
//!
//! Each variant carries the minimum data needed for an MCP tool to
//! reconstruct the full PG row without a second round trip — file id,
//! kind, visibility, etc. — keeping query latency at one disk seek
//! into the trie's mmap region.

use libdictenstein::DictionaryValue;
use serde::{Deserialize, Serialize};

/// Symbol-index payload. Built from `file_symbols` (Shadow-ASR table)
/// rows; one entry per `(file_id, name)`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SymbolValue {
    pub file_id: i64,
    pub kind: String,
    pub visibility: String,
    pub line: i32,
}

impl DictionaryValue for SymbolValue {}

/// Path-index payload. Built from `indexed_files.relative_path`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PathValue {
    pub file_id: i64,
    pub project_id: i32,
    pub size_bytes: i64,
}

impl DictionaryValue for PathValue {}

/// Commit-index payload. Built from `git_commits.subject`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CommitRef {
    pub commit_id: i64,
    pub project_id: i32,
    pub sha: String,
}

impl DictionaryValue for CommitRef {}

/// Durable-mandate index payload. Built from `durable_mandates.imperative`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DurableMandateRef {
    pub mandate_id: i64,
    pub scope: String,
    pub polarity: String,
}

impl DictionaryValue for DurableMandateRef {}
