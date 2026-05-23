//! `tool_injection_candidates` — String concat into exec/query (SOTA Phase 6.5).

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::InjectionCandidatesParams;
use crate::mcp::tools::sema_helpers::effects::symbols_with_effect;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::mcp::tools::sota_regex_scan::scan_files_for_pattern;
use crate::parsing::type_tags::vocabulary::EFFECT_DATABASE;

pub async fn tool_injection_candidates(
    ctx: &SystemContext,
    params: InjectionCandidatesParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "injection_candidates", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50);

    let kind = params.kind.as_deref().unwrap_or("all");
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
    let hits = scan_files_for_pattern(pool, project_id, &pat, None, limit.max(0) as usize)
        .await
        .map_err(|e| McpError::internal_error(format!("Scan failed: {}", e), None))?;
    let rows: Vec<_> = hits
        .into_iter()
        .map(|h| json!({"file": h.relative_path, "language": h.language, "line": h.line, "snippet": h.snippet}))
        .collect();
    // Shadow-ASR channel: symbols carrying the `database` effect.
    let database_effect_symbols = symbols_with_effect(pool, project_id, EFFECT_DATABASE)
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
        "project": params.project,
        "kind": kind,
        "matches": rows,
        "database_effect_symbols": database_effect_symbols,
        "guidance": "String concatenation into SQL/shell commands is the classic injection vector. Use parameterized queries / argv arrays / exec without shell."
    }))
}
