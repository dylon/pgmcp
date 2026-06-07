//! `tool_injection_candidates` — command/SQL/eval injection (graph-roadmap Phase 2.1).
//!
//! `injection_findings` are real source→sink flows (from the shared taint
//! engine) whose sink is an injection class (command / sql / eval) — high
//! confidence. `heuristic_matches` keep the previous regex string-concat-into-
//! exec/query scan for languages without a def-use backend, as review
//! candidates.

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::InjectionCandidatesParams;
use crate::mcp::tools::sema_helpers::effects::symbols_with_effect_limited;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::mcp::tools::sota_regex_scan::scan_files_for_pattern;
use crate::mcp::tools::tool_taint_analysis::scan_project_dataflow;
use crate::parsing::type_tags::vocabulary::EFFECT_DATABASE;

const DEFAULT_INJECTION_FINDING_LIMIT: i32 = 50;
const MAX_INJECTION_FINDING_LIMIT: i32 = 500;

pub async fn tool_injection_candidates(
    ctx: &SystemContext,
    params: InjectionCandidatesParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "injection_candidates", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project = params.project.trim();
    let project_id = project_id_or_err(ctx, project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_INJECTION_FINDING_LIMIT)
        .clamp(1, MAX_INJECTION_FINDING_LIMIT) as usize;

    let kind = params.kind.as_deref().unwrap_or("all").trim();
    // Which sink classes count as "injection" for this request.
    let allowed: &[&str] = match kind {
        "sql" => &["sql"],
        "shell" => &["command"],
        "all" => &["command", "sql", "eval"],
        other => {
            return Err(McpError::invalid_params(
                format!("kind must be one of: all, sql, shell; got '{other}'"),
                None,
            ));
        }
    };

    // High-confidence: real source→sink flows into an injection sink.
    let (intra_hits, interproc_hits) =
        scan_project_dataflow(pool, project_id, limit, Some(allowed))
            .await
            .map_err(|e| McpError::internal_error(format!("Dataflow scan failed: {}", e), None))?;
    let mut injection_findings: Vec<serde_json::Value> = intra_hits
        .into_iter()
        .map(|h| {
            json!({
                "file": h.path,
                "language": h.language,
                "function": h.finding.function,
                "source_kind": h.finding.source_kind,
                "source_line": h.finding.source_line,
                "sink_kind": h.finding.sink_kind,
                "sink_callee": h.finding.sink_callee,
                "sink_line": h.finding.sink_line,
                "interprocedural": false,
            })
        })
        .collect();
    // Interprocedural injection candidates (Phase 3.4): a tainted argument that
    // a called helper routes to an injection sink.
    injection_findings.extend(interproc_hits.into_iter().map(|h| {
        json!({
            "file": h.path,
            "language": h.language,
            "function": h.finding.caller,
            "source_kind": h.finding.source_kind,
            "source_line": h.finding.source_line,
            "sink_kind": h.finding.sink_kind,
            "sink_callee": h.finding.callee,
            "sink_line": h.finding.call_line,
            "interprocedural": true,
        })
    }));
    injection_findings.truncate(limit);

    // Review-candidate heuristic: string concatenation into exec/query (regex),
    // for languages without a def-use backend.
    let pat = match kind {
        "sql" => Regex::new(
            r#"(?mi)(format!\([^)]*"SELECT|format!\([^)]*"INSERT|format!\([^)]*"UPDATE|format!\([^)]*"DELETE|f"SELECT.+\{|f"INSERT.+\{|exec\(\s*['"]SELECT|query\(\s*['"][^"']*\+|execute\(\s*['"][^"']*\+)"#,
        ),
        "shell" => Regex::new(
            r#"(?mi)(Command::new\(\s*['"]sh['"]\s*\)\.args\([^)]*\+|subprocess\.run\(\s*['"][^'"]*['"]\s*\+|os\.system\(\s*['"][^'"]*\+|shell_exec\(\s*['"][^'"]*\+|Runtime\.exec\(\s*['"][^'"]*\+)"#,
        ),
        _ => Regex::new(
            r#"(?mi)(format!\(\s*['"][^'"]*"\s*\+|f"[^"]*\{[^}]*\}[^"]*"\s*\.into\(\)|exec\(\s*['"][^'"]*['"]\s*\+|query\(\s*['"][^'"]*['"]\s*\+|subprocess\.run\(\s*['"][^'"]*\+|os\.system\(\s*['"][^'"]*\+)"#,
        ),
    }
    .expect("inj regex");
    let hits = scan_files_for_pattern(pool, project_id, &pat, None, limit)
        .await
        .map_err(|e| McpError::internal_error(format!("Scan failed: {}", e), None))?;
    let heuristic_matches: Vec<_> = hits
        .into_iter()
        .map(|h| json!({"file": h.relative_path, "language": h.language, "line": h.line, "snippet": h.snippet}))
        .collect();

    let effect_symbol_limit = i64::from(MAX_INJECTION_FINDING_LIMIT);
    let database_effect_symbols = symbols_with_effect_limited(
        pool,
        project_id,
        EFFECT_DATABASE,
        effect_symbol_limit,
    )
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(symbol_id, file_id, name, scope_path)| {
        serde_json::json!({
            "symbol_id": symbol_id, "file_id": file_id, "name": name, "scope_path": scope_path,
        })
    })
    .collect::<Vec<_>>();

    json_result(&json!({
        "project": project,
        "kind": kind,
        "limit": limit,
        "effect_symbol_limit": effect_symbol_limit,
        "injection_findings": injection_findings,
        "heuristic_matches": heuristic_matches,
        "database_effect_symbols": database_effect_symbols,
        "guidance": "`injection_findings` are REAL source→sink flows into a command/SQL/eval sink (Rust): \
            attacker-controllable input provably reaches the sink without a sanitizer — fix with parameterized \
            queries / argv arrays / exec-without-shell. `heuristic_matches` are regex string-concat-into-exec/query \
            hits for languages without a def-use backend — review candidates. `database_effect_symbols` lists \
            symbols carrying the database effect."
    }))
}
