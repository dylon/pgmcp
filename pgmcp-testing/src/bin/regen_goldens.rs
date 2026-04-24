//! Bulk regenerator for the golden-fixture suite.
//!
//! Walks every registered generator in `GENERATORS`, writes the
//! corresponding `.postcard` fixture file, and prints a one-line
//! status per fixture (`new`, `unchanged`, `updated`). Exits non-zero
//! iff any fixture changed, so CI can gate on an unstaged diff.
//!
//! Usage:
//!
//! ```text
//! cargo run --release -p pgmcp-testing --bin regen-goldens
//! ```
//!
//! Add a new fixture by:
//!
//! 1. Adding a generator function in this file that builds `(input,
//!    output, tolerance)` for the component.
//! 2. Appending a `(name, generator_fn)` entry to the `GENERATORS`
//!    slice below.
//! 3. Running `cargo run --release -p pgmcp-testing --bin
//!    regen-goldens`. The new fixture is reported as `new`.
//! 4. Committing the new `.postcard` file alongside the generator.

use ndarray::{Array2, ArrayView2};
use pgmcp::config::merge_toml_values;
use pgmcp::cron::topic_clustering::{self, TopicKeyword};
use pgmcp::fcm::FcmResult;
use pgmcp::graph::import_extractor::{self, RawImport};
use pgmcp::indexer::chunker::{self, Chunk};
use pgmcp::indexer::claude_chunker::{self};
use pgmcp_testing::golden::{RegenStatus, regen_golden};

fn main() -> std::process::ExitCode {
    println!("== regenerating pgmcp golden fixtures ==");
    let mut changed = 0u32;
    let mut total = 0u32;
    for (name, run) in GENERATORS {
        let status = run(name);
        total += 1;
        if !matches!(status, RegenStatus::Unchanged) {
            changed += 1;
        }
        println!("  {:10}  {}", status.to_string(), name);
    }
    println!("---\n{} total, {} changed", total, changed);
    if changed == 0 {
        std::process::ExitCode::SUCCESS
    } else {
        // Non-zero exit gives CI a signal to fail when uncommitted
        // goldens appear. The developer is expected to inspect the
        // diff and commit the new fixtures.
        std::process::ExitCode::from(2)
    }
}

/// One generator per fixture. The fn pointer takes the fixture name
/// (for error messages) and returns the `RegenStatus` produced by
/// [`regen_golden`].
type Generator = fn(&str) -> RegenStatus;

/// The full registry. New fixtures go here.
const GENERATORS: &[(&str, Generator)] = &[
    // chunker (discrete)
    ("chunker/short_rust_file", regen_chunker_short_rust_file),
    (
        "chunker/long_rust_with_overlap",
        regen_chunker_long_rust_with_overlap,
    ),
    ("chunker/single_line", regen_chunker_single_line),
    ("chunker/crlf_content", regen_chunker_crlf_content),
    ("chunker/jsonl_mixed", regen_chunker_jsonl_mixed),
    // claude_chunker
    (
        "claude_chunker/session_basic",
        regen_claude_chunker_session_basic,
    ),
    (
        "claude_chunker/session_mixed_types",
        regen_claude_chunker_session_mixed_types,
    ),
    // import_extractor (per language)
    ("import_extractor/rust_use_mod_extern", regen_import_rust),
    ("import_extractor/python_import_from", regen_import_python),
    (
        "import_extractor/javascript_import_require",
        regen_import_javascript,
    ),
    ("import_extractor/java_import", regen_import_java),
    ("import_extractor/go_import", regen_import_go),
    ("import_extractor/c_include", regen_import_c),
    // merge_toml
    (
        "merge_toml/tables_user_wins_scalars",
        regen_merge_toml_tables,
    ),
    ("merge_toml/arrays_union", regen_merge_toml_arrays),
    // c-TF-IDF (float)
    (
        "ctf_idf/three_topics_ten_chunks",
        regen_ctf_idf_three_topics,
    ),
    // FCM (float)
    ("fcm/two_blobs_seed_42", regen_fcm_two_blobs_seed_42),
];

// ============================================================================
// chunker goldens
// ============================================================================

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChunkerInput {
    pub content: String,
    pub chunk_size_lines: usize,
    pub chunk_overlap_lines: usize,
}

/// Inputs to the JSONL chunker have no knobs — just a string.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JsonlChunkerInput {
    pub content: String,
}

fn regen_chunker_short_rust_file(name: &str) -> RegenStatus {
    let content = (1..=20)
        .map(|i| format!("fn function_{}() {{ println!(\"line {}\"); }}", i, i))
        .collect::<Vec<_>>()
        .join("\n");
    let input = ChunkerInput {
        content,
        chunk_size_lines: 10,
        chunk_overlap_lines: 2,
    };
    let output: Vec<Chunk> = chunker::chunk_content(
        &input.content,
        input.chunk_size_lines,
        input.chunk_overlap_lines,
    );
    regen_golden(name, input, output, None)
}

fn regen_chunker_long_rust_with_overlap(name: &str) -> RegenStatus {
    let content = (1..=80)
        .map(|i| format!("line {:02}: stable_synthetic_content_token_{}", i, i % 7))
        .collect::<Vec<_>>()
        .join("\n");
    let input = ChunkerInput {
        content,
        chunk_size_lines: 25,
        chunk_overlap_lines: 5,
    };
    let output: Vec<Chunk> = chunker::chunk_content(
        &input.content,
        input.chunk_size_lines,
        input.chunk_overlap_lines,
    );
    regen_golden(name, input, output, None)
}

fn regen_chunker_single_line(name: &str) -> RegenStatus {
    let input = ChunkerInput {
        content: "a single line of content with no newlines".to_string(),
        chunk_size_lines: 10,
        chunk_overlap_lines: 2,
    };
    let output: Vec<Chunk> = chunker::chunk_content(
        &input.content,
        input.chunk_size_lines,
        input.chunk_overlap_lines,
    );
    regen_golden(name, input, output, None)
}

fn regen_chunker_crlf_content(name: &str) -> RegenStatus {
    // Multi-chunk so the rejoin path is exercised (single-chunk path
    // preserves CRLF verbatim; multi-chunk normalises to LF).
    let lines: Vec<String> = (1..=12).map(|i| format!("crlf line {}\r", i)).collect();
    let input = ChunkerInput {
        content: lines.join("\n"),
        chunk_size_lines: 5,
        chunk_overlap_lines: 1,
    };
    let output: Vec<Chunk> = chunker::chunk_content(
        &input.content,
        input.chunk_size_lines,
        input.chunk_overlap_lines,
    );
    regen_golden(name, input, output, None)
}

fn regen_chunker_jsonl_mixed(name: &str) -> RegenStatus {
    // Each non-blank line becomes one chunk for the JSONL chunker.
    let input = JsonlChunkerInput {
        content: concat!(
            "{\"a\": 1}\n",
            "\n",    // blank → skipped
            "   \n", // whitespace-only → skipped
            "{\"b\": 2}\n",
            "{\"c\": 3}\n",
        )
        .to_string(),
    };
    let output: Vec<Chunk> = chunker::chunk_jsonl_content(&input.content);
    regen_golden(name, input, output, None)
}

// ============================================================================
// claude_chunker goldens
// ============================================================================

fn regen_claude_chunker_session_basic(name: &str) -> RegenStatus {
    let input = JsonlChunkerInput {
        content: concat!(
            "{\"type\": \"user\", \"message\": \"How do I fix this bug?\"}\n",
            "{\"type\": \"assistant\", \"message\": \"Here is the fix ...\"}\n",
        )
        .to_string(),
    };
    let output: Vec<Chunk> = claude_chunker::chunk_claude_jsonl(&input.content);
    regen_golden(name, input, output, None)
}

fn regen_claude_chunker_session_mixed_types(name: &str) -> RegenStatus {
    let input = JsonlChunkerInput {
        content: concat!(
            "{\"type\": \"user\", \"message\": \"ping\"}\n",
            "{\"type\": \"progress\", \"data\": \"...\"}\n", // skipped
            "{\"type\": \"assistant\", \"message\": \"pong\"}\n",
            "{\"type\": \"file-history-snapshot\", \"filePath\": \"/x.rs\", \"backupFileName\": \"abc@v1\"}\n", // skipped
            "{\"type\": \"tool_result\", \"name\": \"Read\", \"result\": \"file bytes\"}\n",
        )
        .to_string(),
    };
    let output: Vec<Chunk> = claude_chunker::chunk_claude_jsonl(&input.content);
    regen_golden(name, input, output, None)
}

// ============================================================================
// import_extractor goldens (per language)
// ============================================================================

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ImportInput {
    pub content: String,
    pub language: String,
}

fn regen_import_rust(name: &str) -> RegenStatus {
    let input = ImportInput {
        content: concat!(
            "use crate::db::queries::list_projects;\n",
            "use crate::indexer::chunker;\n",
            "pub mod config;\n",
            "mod private_helper;\n",
            "extern crate serde;\n",
            "fn main() {}\n",
        )
        .to_string(),
        language: "rust".into(),
    };
    let output: Vec<RawImport> = import_extractor::extract_imports(&input.content, &input.language);
    regen_golden(name, input, output, None)
}

fn regen_import_python(name: &str) -> RegenStatus {
    let input = ImportInput {
        content: concat!(
            "import os\n",
            "import sys.path\n",
            "from pathlib import Path\n",
            "from collections.abc import Mapping\n",
        )
        .to_string(),
        language: "python".into(),
    };
    let output: Vec<RawImport> = import_extractor::extract_imports(&input.content, &input.language);
    regen_golden(name, input, output, None)
}

fn regen_import_javascript(name: &str) -> RegenStatus {
    let input = ImportInput {
        content: concat!(
            "import { foo } from 'lodash';\n",
            "import bar from './bar.js';\n",
            "const fs = require('fs');\n",
            "export { baz } from './baz';\n",
        )
        .to_string(),
        language: "javascript".into(),
    };
    let output: Vec<RawImport> = import_extractor::extract_imports(&input.content, &input.language);
    regen_golden(name, input, output, None)
}

fn regen_import_java(name: &str) -> RegenStatus {
    let input = ImportInput {
        content: concat!(
            "package com.example;\n",
            "import java.util.List;\n",
            "import java.util.Map;\n",
            "import static java.lang.Math.PI;\n",
        )
        .to_string(),
        language: "java".into(),
    };
    let output: Vec<RawImport> = import_extractor::extract_imports(&input.content, &input.language);
    regen_golden(name, input, output, None)
}

fn regen_import_go(name: &str) -> RegenStatus {
    let input = ImportInput {
        content: concat!(
            "package main\n",
            "\n",
            "import (\n",
            "    \"fmt\"\n",
            "    \"os\"\n",
            "    \"github.com/pkg/errors\"\n",
            ")\n",
        )
        .to_string(),
        language: "go".into(),
    };
    let output: Vec<RawImport> = import_extractor::extract_imports(&input.content, &input.language);
    regen_golden(name, input, output, None)
}

fn regen_import_c(name: &str) -> RegenStatus {
    let input = ImportInput {
        content: concat!(
            "#include <stdio.h>\n",
            "#include <stdlib.h>\n",
            "#include \"local_header.h\"\n",
        )
        .to_string(),
        language: "c".into(),
    };
    let output: Vec<RawImport> = import_extractor::extract_imports(&input.content, &input.language);
    regen_golden(name, input, output, None)
}

// ============================================================================
// merge_toml goldens
// ============================================================================

/// Inputs to `merge_toml_values` are TOML source strings. Stored as
/// strings so the fixture is human-readable if anyone inspects the
/// postcard bytes.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MergeTomlInput {
    pub defaults: String,
    pub user: String,
}

fn run_merge(input: &MergeTomlInput) -> String {
    let defaults: toml::Value = toml::from_str(&input.defaults).expect("parse defaults");
    let user: toml::Value = toml::from_str(&input.user).expect("parse user");
    let merged = merge_toml_values(defaults, user);
    toml::to_string_pretty(&merged).expect("re-serialize merged")
}

fn regen_merge_toml_tables(name: &str) -> RegenStatus {
    let input = MergeTomlInput {
        defaults: concat!(
            "[database]\n",
            "host = \"localhost\"\n",
            "port = 5432\n",
            "name = \"pgmcp_default\"\n",
        )
        .to_string(),
        user: concat!("[database]\n", "name = \"pgmcp_custom\"\n").to_string(),
    };
    let output = run_merge(&input);
    regen_golden(name, input, output, None)
}

fn regen_merge_toml_arrays(name: &str) -> RegenStatus {
    let input = MergeTomlInput {
        defaults: "paths = [\"/a\", \"/b\", \"/c\"]\n".into(),
        user: "paths = [\"/user_a\", \"/a\"]\n".into(),
    };
    let output = run_merge(&input);
    regen_golden(name, input, output, None)
}

// ============================================================================
// c-TF-IDF golden (float)
// ============================================================================

/// Inputs to c-TF-IDF: text chunks + membership matrix + top_k.
///
/// Membership stored as `Array2<f32>` so serialization is one-shot via
/// `ndarray/serde` and there's no n/k drift between the value and the
/// declared shape.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CtfIdfInput {
    pub contents: Vec<String>,
    pub membership: Array2<f32>,
    pub top_k: usize,
}

fn run_ctf_idf(input: &CtfIdfInput) -> Vec<Vec<TopicKeyword>> {
    let refs: Vec<&str> = input.contents.iter().map(|s| s.as_str()).collect();
    topic_clustering::compute_ctf_idf(&refs, &input.membership, input.top_k)
}

fn regen_ctf_idf_three_topics(name: &str) -> RegenStatus {
    // Three topics, ten chunks. Membership is hand-chosen as a hard
    // assignment with a small "leak" mass (0.05) into the next topic to
    // exercise the weighted-token branch (mu > 1e-8 but < 1.0).
    //
    // Chunks 0..3 → topic 0 (auth)
    // Chunks 4..7 → topic 1 (database)
    // Chunks 8..9 → topic 2 (logging)
    let contents: Vec<String> = vec![
        "validate password hash with bcrypt and verify token signature".into(),
        "issue access token after password verification flow completes".into(),
        "refresh expired token using stored refresh credential".into(),
        "reject invalid password and lock account after failures".into(),
        "execute query against postgres connection pool with retries".into(),
        "open transaction and commit row insertion to database".into(),
        "rollback database transaction when query returns conflict".into(),
        "select rows from postgres table joining on indexed column".into(),
        "emit log message at info level with structured fields".into(),
        "warn log on slow request handler exceeding latency threshold".into(),
    ];
    let n = contents.len();
    let k = 3;
    let mut membership = Array2::<f32>::zeros((n, k));
    for (i, row) in (0..n).map(|i| (i, primary_topic_for(i))).enumerate() {
        let primary = row.1;
        let leak = (primary + 1) % k;
        membership[[i, primary]] = 0.95;
        membership[[i, leak]] = 0.05;
    }
    let input = CtfIdfInput {
        contents,
        membership,
        top_k: 5,
    };
    let output = run_ctf_idf(&input);
    regen_golden(name, input, output, Some(1e-10))
}

/// Topic assignment plan for `regen_ctf_idf_three_topics`. Pulled out so
/// the generator stays declarative and the partition is reviewable at a
/// glance.
fn primary_topic_for(chunk_idx: usize) -> usize {
    match chunk_idx {
        0..=3 => 0, // auth
        4..=7 => 1, // database
        _ => 2,     // logging
    }
}

// ============================================================================
// FCM golden (float)
// ============================================================================

/// Inputs to FCM: data matrix + hyperparameters + RNG seed. The seed
/// makes k-means++ initialization deterministic, so the resulting
/// centroids are reproducible up to GEMM rounding (<1e-5 per cell on
/// CPU; the CPU backend is what runs here since `fuzzy_c_means_seeded`
/// pins `BackendChoice::Cpu`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FcmInput {
    pub data: Array2<f32>,
    pub k: usize,
    pub fuzziness: f64,
    pub max_iters: usize,
    pub tolerance: f64,
    pub seed: u64,
}

fn run_fcm(input: &FcmInput) -> FcmResult {
    topic_clustering::fuzzy_c_means_seeded(
        ArrayView2::from(&input.data),
        input.k,
        input.fuzziness,
        input.max_iters,
        input.tolerance,
        input.seed,
    )
}

fn regen_fcm_two_blobs_seed_42(name: &str) -> RegenStatus {
    // Two well-separated blobs in 3-D, 15 points each. The blobs are
    // far enough apart (centers (0,0,0) vs (10,10,10), spread 0.1) that
    // FCM should converge to a hard-ish two-cluster assignment in a
    // handful of iterations regardless of seed — the seed is here to
    // pin which centroid index maps to which blob.
    let mut points: Vec<f32> = Vec::with_capacity(30 * 3);
    let blob_a_center = [0.0_f32, 0.0, 0.0];
    let blob_b_center = [10.0_f32, 10.0, 10.0];
    // Quasi-random offsets — deterministic, no RNG dependency in the
    // generator (the RNG seed is for k-means++, not for the data).
    let offsets: [[f32; 3]; 15] = [
        [0.10, -0.05, 0.02],
        [-0.07, 0.04, 0.09],
        [0.03, 0.08, -0.06],
        [0.05, -0.10, 0.01],
        [-0.02, 0.07, 0.05],
        [0.08, 0.03, -0.04],
        [-0.09, -0.01, 0.06],
        [0.04, 0.06, 0.10],
        [0.07, -0.08, -0.03],
        [-0.05, 0.02, 0.04],
        [0.06, -0.06, 0.08],
        [-0.03, 0.09, -0.02],
        [0.09, 0.05, 0.03],
        [-0.04, -0.07, -0.05],
        [0.02, 0.01, 0.07],
    ];
    for off in &offsets {
        points.push(blob_a_center[0] + off[0]);
        points.push(blob_a_center[1] + off[1]);
        points.push(blob_a_center[2] + off[2]);
    }
    for off in &offsets {
        points.push(blob_b_center[0] + off[0]);
        points.push(blob_b_center[1] + off[1]);
        points.push(blob_b_center[2] + off[2]);
    }
    let data = Array2::from_shape_vec((30, 3), points).expect("30x3 from flat");
    let input = FcmInput {
        data,
        k: 2,
        fuzziness: 2.0,
        max_iters: 50,
        tolerance: 1e-5,
        seed: 42,
    };
    let output = run_fcm(&input);
    regen_golden(name, input, output, Some(1e-5))
}
