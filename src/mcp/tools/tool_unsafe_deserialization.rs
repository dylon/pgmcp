//! `tool_unsafe_deserialization` — CWE-502 patterns (graph-roadmap Phase 2.2).
//!
//! `ast_findings` are AST-matched (tree-sitter): precise, immune to matches in
//! comments/strings, and shape-aware (`yaml.load` without a safe `Loader=`).
//! `heuristic_matches` keep the regex scan for languages without an AST rule
//! set.

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::code_analysis::ast_rules;
use crate::context::SystemContext;
use crate::mcp::server::UnsafeDeserializationParams;
use crate::mcp::tools::sema_helpers::effects::symbols_with_effect;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::mcp::tools::sota_regex_scan::scan_files_for_pattern;
use crate::mcp::tools::tool_crypto_misuse::scan_project_ast_rules;
use crate::parsing::type_tags::vocabulary::EFFECT_UNSAFE;

pub async fn tool_unsafe_deserialization(
    ctx: &SystemContext,
    params: UnsafeDeserializationParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "unsafe_deserialization", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50).max(0) as usize;

    // Precise AST findings (deserialize category).
    let mut ast_findings: Vec<serde_json::Value> = scan_project_ast_rules(pool, project_id)
        .await
        .map_err(|e| McpError::internal_error(format!("AST scan failed: {}", e), None))?
        .into_iter()
        .filter(|(_, _, h)| h.category == "deserialize")
        .map(|(path, lang, h)| {
            json!({
                "rule": h.rule_id, "file": path, "language": lang,
                "line": h.line, "message": h.message, "snippet": h.snippet,
            })
        })
        .collect();
    ast_findings.truncate(limit);

    // Regex heuristic for languages without an AST rule set.
    let pat = Regex::new(
        r"(?m)\b(pickle\.loads|pickle\.load|cPickle\.loads|yaml\.load\s*\([^)]*\)|ObjectInputStream|unserialize\(|jsonpickle\.decode|marshal\.loads|shelve\.open|Marshal\.load|dill\.loads|deserialize_object)\b"
    ).expect("deserial regex");
    let hits = scan_files_for_pattern(pool, project_id, &pat, None, limit)
        .await
        .map_err(|e| McpError::internal_error(format!("Scan failed: {}", e), None))?;
    let heuristic_matches: Vec<_> = hits
        .into_iter()
        .filter(|h| !ast_rules::has_rules(&h.language))
        .map(|h| json!({"file": h.relative_path, "language": h.language, "line": h.line, "snippet": h.snippet}))
        .collect();

    let effect_symbols = symbols_with_effect(pool, project_id, EFFECT_UNSAFE)
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
        "ast_findings": ast_findings,
        "heuristic_matches": heuristic_matches,
        "effect_symbols": effect_symbols,
        "guidance": "`ast_findings` are precise tree-sitter matches (pickle/marshal load, yaml.load without a \
            safe Loader) — they never fire on comments/strings. `heuristic_matches` are the regex scan for \
            languages without an AST rule set. Unsafe deserialization executes arbitrary code from \
            attacker-controlled bytes (CWE-502); replace with json, yaml.safe_load, or typed Serde."
    }))
}
