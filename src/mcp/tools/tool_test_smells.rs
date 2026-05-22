//! `tool_test_smells` — Detect canonical test smells (SOTA Phase 4.5,
//! van Deursen et al. XP 2001; Garousi et al. JSS 2018).
//!
//! Assertion Roulette, Mystery Guest, Eager Test, Conditional Logic in Tests,
//! Resource Optimism. Per-language regex-on-content heuristics applied to test
//! files (path matches /test/ or *_test.* or *_spec.* or tests/*).

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::TestSmellsParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

static TEST_PATH_RE: OnceLock<Regex> = OnceLock::new();
static ASSERT_RE: OnceLock<Regex> = OnceLock::new();
static IF_OR_FOR_RE: OnceLock<Regex> = OnceLock::new();
static FILE_OPEN_RE: OnceLock<Regex> = OnceLock::new();
static TEST_FN_RE: OnceLock<Regex> = OnceLock::new();

fn test_path_re() -> &'static Regex {
    TEST_PATH_RE.get_or_init(|| {
        Regex::new(r"(?i)(^|/)(test|tests|spec|specs)(/|_)|(_test|_spec)\.[a-z]+$")
            .expect("test_path regex")
    })
}
fn assert_re() -> &'static Regex {
    ASSERT_RE.get_or_init(|| {
        Regex::new(r"(?m)\b(assert(_eq|_ne|_matches|_approx_eq)?!?|expect|should)\b")
            .expect("assert regex")
    })
}
fn if_or_for_re() -> &'static Regex {
    IF_OR_FOR_RE
        .get_or_init(|| Regex::new(r"(?m)^\s*(if|for|while|match)\s+").expect("if/for regex"))
}
fn file_open_re() -> &'static Regex {
    FILE_OPEN_RE.get_or_init(|| {
        Regex::new(r"(?m)\b(File::open|open\(|fs::read|read_to_string|read_file)\b")
            .expect("file open regex")
    })
}
fn test_fn_re() -> &'static Regex {
    TEST_FN_RE.get_or_init(|| {
        Regex::new(r"(?m)#\[(?:tokio::)?test\b|^\s*def\s+test_|^\s*it\(|^\s*test\(")
            .expect("test fn regex")
    })
}

pub async fn tool_test_smells(
    ctx: &SystemContext,
    params: TestSmellsParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "test_smells", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    let rows: Vec<(String, Option<String>)> = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT relative_path, content
         FROM indexed_files
         WHERE project_id = $1 AND content IS NOT NULL",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("File query failed: {}", e), None))?;

    let limit = params.limit.unwrap_or(50);
    let mut findings: Vec<serde_json::Value> = Vec::new();
    for (path, content) in rows {
        if !test_path_re().is_match(&path) {
            continue;
        }
        let content = match content {
            Some(c) => c,
            None => continue,
        };
        // Split into per-test bodies via naive #[test]/def/it/test() splitter.
        let test_fn_count = test_fn_re().find_iter(&content).count();
        if test_fn_count == 0 {
            continue;
        }
        let asserts = assert_re().find_iter(&content).count();
        let ifs = if_or_for_re().find_iter(&content).count();
        let opens = file_open_re().find_iter(&content).count();
        let asserts_per_test = asserts as f64 / test_fn_count.max(1) as f64;
        let mut smells: Vec<&str> = Vec::new();
        if asserts_per_test >= 5.0 {
            smells.push("assertion_roulette");
        }
        if ifs >= test_fn_count {
            smells.push("conditional_logic");
        }
        if opens > 0 {
            smells.push("mystery_guest");
        }
        if test_fn_count >= 5 && asserts_per_test >= 3.0 {
            smells.push("eager_test");
        }
        if !smells.is_empty() {
            findings.push(json!({
                "file": path,
                "test_count": test_fn_count,
                "asserts": asserts,
                "if_or_for": ifs,
                "external_reads": opens,
                "smells": smells,
            }));
        }
    }
    findings.sort_by(|a, b| {
        let av = a["smells"].as_array().map(|x| x.len()).unwrap_or(0);
        let bv = b["smells"].as_array().map(|x| x.len()).unwrap_or(0);
        bv.cmp(&av)
    });
    findings.truncate(limit.max(0) as usize);
    json_result(&json!({
        "project": params.project,
        "findings": findings,
        "guidance": "Each smell follows van Deursen et al. XP 2001. assertion_roulette = many asserts in one test (unclear failure cause); mystery_guest = external file/state read; conditional_logic = if/for inside tests; eager_test = multiple acts per test."
    }))
}
