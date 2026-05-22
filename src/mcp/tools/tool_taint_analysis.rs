//! `tool_taint_analysis` — Source→sink pattern detection (SOTA Phase 6.1, Newsome-Song NDSS 2005).
//!
//! Heuristic: list lines that match a taint *source* (request input, env var, file read)
//! and lines that match a taint *sink* (exec, raw SQL, eval, format string into shell)
//! in the same file. A real CFG-based taint analysis requires call-graph + data-flow
//! tracking; this tool surfaces high-risk co-occurrences for manual review.

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::TaintAnalysisParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_taint_analysis(
    ctx: &SystemContext,
    params: TaintAnalysisParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "taint_analysis", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    let source_re = Regex::new(
        r"(?m)\b(req\.body|req\.params|req\.query|request\.json|request\.form|request\.args|argv|env::var|std::env::var|getenv|input\(\)|stdin)\b",
    )
    .expect("source regex");
    let sink_re = Regex::new(
        r"(?m)\b(Command::new|exec\(|eval\(|spawn_shell|subprocess\.run|os\.system|sql\.query\(|execute\(|Runtime\.exec|shell_exec|sqlx::query_unchecked)\b",
    )
    .expect("sink regex");

    let rows: Vec<(String, String, Option<String>)> =
        sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT relative_path, language, content
             FROM indexed_files
             WHERE project_id = $1 AND content IS NOT NULL",
        )
        .bind(project_id)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Scan failed: {}", e), None))?;

    let limit = params.limit.unwrap_or(30);
    let mut findings: Vec<serde_json::Value> = Vec::new();
    for (path, lang, content) in rows {
        let Some(c) = content else { continue };
        let sources: Vec<u32> = source_re
            .find_iter(&c)
            .map(|m| c[..m.start()].bytes().filter(|b| *b == b'\n').count() as u32 + 1)
            .collect();
        let sinks: Vec<u32> = sink_re
            .find_iter(&c)
            .map(|m| c[..m.start()].bytes().filter(|b| *b == b'\n').count() as u32 + 1)
            .collect();
        if sources.is_empty() || sinks.is_empty() {
            continue;
        }
        findings.push(json!({
            "file": path,
            "language": lang,
            "source_lines": sources,
            "sink_lines": sinks,
        }));
        if findings.len() >= limit.max(0) as usize {
            break;
        }
    }
    json_result(&json!({
        "project": params.project,
        "findings": findings,
        "guidance": "Files where both sources (request/env/stdin) and sinks (exec/eval/SQL) appear are taint candidates. Manual review needed to confirm flow."
    }))
}
