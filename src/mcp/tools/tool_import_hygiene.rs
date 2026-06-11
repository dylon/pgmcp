//! `tool_import_hygiene` — flag imports embedded inside function / method /
//! lambda bodies instead of at the top of the file or module.
//!
//! Pure shadow-ASR analysis. `nested_import_violations` joins each persisted
//! `import_use` reference to its resolved enclosing symbol (`source_symbol_id`,
//! filled by the `symbol-extraction` cron's `resolve_source_symbol_ids`) and keeps
//! only callable enclosers. A `use`/`import` at a file root resolves to no symbol
//! and one at a module / test-module top resolves to that `module`; neither is
//! flagged — so the policy "file-top and `mod tests { … }`-top imports are fine,
//! function-body imports are not" falls out directly. Cross-language: every backend
//! that emits `import_use` rows participates.
//!
//! `severity`/`duplication` ride `dup_count` (how many function bodies in the same
//! file re-type the same import) — the strongest "hoist me to the top" signal and
//! the duplicated `use` lines the check exists to eliminate.
//!
//! Soft-fails (never errors) when the project is unknown or the symbol-extraction
//! cron hasn't populated/resolved imports yet, with `health.symbols_present:false`.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::mcp::tools::fix_helpers::{lookup_project_id, pool_or_err};

pub async fn tool_import_hygiene(
    ctx: &SystemContext,
    params: ImportHygieneParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .import_hygiene_scans
        .fetch_add(1, Ordering::Relaxed);

    let limit = params.limit.unwrap_or(100).clamp(1, 1000) as usize;

    debug!(
        tool = "import_hygiene",
        project = %params.project,
        language = ?params.language,
        limit,
        "MCP tool invoked",
    );

    let project_id = match lookup_project_id(ctx, &params.project).await? {
        Some(id) => id,
        None => return empty_envelope(&params, true),
    };

    let pool = pool_or_err(ctx)?;
    let rows =
        crate::db::queries::nested_import_violations(pool, project_id, params.language.as_deref())
            .await
            .map_err(|e| {
                McpError::internal_error(format!("nested_import_violations failed: {e}"), None)
            })?;

    if rows.is_empty() {
        return empty_envelope(&params, false);
    }

    let total = rows.len();

    // Per-file rollup (BTreeMap → deterministic order): violation count + the worst
    // duplication seen in the file, so a consumer can rank files by hygiene debt.
    let mut rollup: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    for r in &rows {
        let e = rollup.entry(r.relative_path.clone()).or_insert((0, 0));
        e.0 += 1;
        e.1 = e.1.max(r.dup_count);
    }
    let by_file: Vec<serde_json::Value> = rollup
        .into_iter()
        .map(|(file, (count, max_dup))| {
            json!({ "file": file, "violations": count, "max_duplication": max_dup })
        })
        .collect();

    let violations: Vec<serde_json::Value> = rows
        .iter()
        .take(limit)
        .map(|r| {
            json!({
                "file": r.relative_path,
                "line": r.source_line,
                "import": r.target_raw,
                "in_symbol": r.enclosing_name,
                "in_kind": r.enclosing_kind,
                "language": r.language,
                "duplication": r.dup_count,
                "severity": severity_label(r.dup_count),
                "guidance": "Move this `use`/`import` to the top of the file or module \
                             (test imports belong at the top of the test module).",
            })
        })
        .collect();

    let result = json!({
        "scope": { "project": params.project, "language": params.language },
        "total_violations": total,
        "returned": violations.len(),
        "by_file": by_file,
        "violations": violations,
        "parameters": { "project": params.project, "language": params.language, "limit": limit },
        "health": { "symbols_present": true },
    });

    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {e}"), None))?;

    debug!(
        tool = "import_hygiene",
        duration_ms = start.elapsed().as_millis() as u64,
        total_violations = total,
        returned = violations.len(),
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}

/// `dup_count`-driven severity label, mirroring the collector's Low/Medium/High ramp.
fn severity_label(dup_count: i64) -> &'static str {
    match dup_count {
        n if n >= 4 => "high",
        n if n >= 2 => "medium",
        _ => "low",
    }
}

/// Empty-result envelope. `unknown_project = true` ⇒ the project isn't indexed
/// (`health.symbols_present:false`). Otherwise there are no nested-import
/// violations *or* the symbol-extraction cron hasn't populated/resolved imports yet
/// — a clean-or-pending state that is genuinely not an error.
fn empty_envelope(
    params: &ImportHygieneParams,
    unknown_project: bool,
) -> Result<CallToolResult, McpError> {
    let guidance = if unknown_project {
        format!(
            "Project `{}` is not indexed. Run `pgmcp scan` or check `list_projects`.",
            params.project
        )
    } else {
        "No imports were found inside function bodies for this project. If you expected \
         findings, the symbol-extraction cron may not have populated/resolved `import_use` \
         references yet (imports resolve their enclosing scope during extraction) — wait for \
         the cron or trigger `symbol-extraction`, then retry."
            .to_string()
    };
    let result = json!({
        "scope": { "project": params.project, "language": params.language },
        "total_violations": 0,
        "returned": 0,
        "by_file": [],
        "violations": [],
        "parameters": { "project": params.project, "language": params.language },
        "guidance": guidance,
        "health": { "symbols_present": !unknown_project },
    });
    let s = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(s)]))
}
