//! `tool_unprotected_routes` — Mutating HTTP routes without visible auth middleware
//! (SOTA Phase 6.6).

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::UnprotectedRoutesParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_unprotected_routes(
    ctx: &SystemContext,
    params: UnprotectedRoutesParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "unprotected_routes", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50);

    // Frameworks: axum (.route("/x", post(...))), Express (app.post), Flask (@app.route methods=POST), Spring (@PostMapping)
    let route_re = Regex::new(
        r"(?m)(\.route\([^)]*post\(|\.route\([^)]*put\(|\.route\([^)]*delete\(|\.route\([^)]*patch\(|app\.(post|put|delete|patch)\(|@(?:Post|Put|Delete|Patch)Mapping|methods=\[?['\x22](POST|PUT|DELETE|PATCH))"
    ).expect("route regex");
    let auth_re = Regex::new(
        r"(?m)(require_auth|require_login|login_required|verify_jwt|require_role|@authenticated|@requires_auth|AuthGuard|RequireAuth|auth\.|@PreAuthorize)"
    ).expect("auth regex");

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

    let mut findings: Vec<serde_json::Value> = Vec::new();
    for (path, lang, content) in rows {
        let Some(c) = content else { continue };
        let has_auth = auth_re.is_match(&c);
        for m in route_re.find_iter(&c) {
            let line = c[..m.start()].bytes().filter(|b| *b == b'\n').count() + 1;
            findings.push(json!({
                "file": path,
                "language": lang,
                "line": line,
                "route": m.as_str(),
                "auth_in_file": has_auth,
            }));
            if findings.len() >= limit.max(0) as usize {
                break;
            }
        }
        if findings.len() >= limit.max(0) as usize {
            break;
        }
    }
    json_result(&json!({
        "project": params.project,
        "matches": findings,
        "guidance": "Routes with mutating verbs (POST/PUT/DELETE/PATCH) in files lacking visible auth middleware are review candidates. A complete check requires per-route middleware-stack inspection beyond regex."
    }))
}
