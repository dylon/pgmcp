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
use crate::mcp::tools::sema_helpers::effects::{symbols_with_all_effects, symbols_with_effect};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::parsing::type_tags::vocabulary::{EFFECT_AUTH_REQUIRED, EFFECT_HTTP_HANDLER};

pub async fn tool_unprotected_routes(
    ctx: &SystemContext,
    params: UnprotectedRoutesParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "unprotected_routes", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50);

    // Frameworks: axum (.route("/x", post(...))), Express (app.post), Flask
    // (@app.route methods=POST), Spring (@PostMapping), and Clojure Ring /
    // Compojure (`(POST "/x" ...)`, `(defroutes ...)`, `(context "/api" ...)`).
    // The Compojure verbs are uppercase macros at the head of a list form, so
    // they're anchored on `(VERB` + whitespace + a string/binding to avoid
    // matching the same tokens used as plain words.
    let route_re = Regex::new(
        r#"(?m)(\.route\([^)]*post\(|\.route\([^)]*put\(|\.route\([^)]*delete\(|\.route\([^)]*patch\(|app\.(post|put|delete|patch)\(|@(?:Post|Put|Delete|Patch)Mapping|methods=\[?['\x22](POST|PUT|DELETE|PATCH)|\(\s*(?:GET|POST|PUT|DELETE|PATCH|ANY)\s+["]|\(\s*(?:defroutes|context)\s)"#
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
    // Shadow-ASR channel: route handler symbols flagged with `http_handler`
    // but NOT carrying `auth_required`. Requires the extractor to emit
    // both effects; degrades to empty when absent.
    let http_handler_symbols = symbols_with_effect(pool, project_id, EFFECT_HTTP_HANDLER)
        .await
        .unwrap_or_default();
    let with_auth = symbols_with_all_effects(
        pool,
        project_id,
        &[
            EFFECT_HTTP_HANDLER.to_string(),
            EFFECT_AUTH_REQUIRED.to_string(),
        ],
    )
    .await
    .unwrap_or_default();
    let auth_set: std::collections::HashSet<i64> =
        with_auth.iter().map(|(id, _, _, _)| *id).collect();
    let http_handler_symbols: Vec<serde_json::Value> = http_handler_symbols
        .into_iter()
        .filter(|(id, _, _, _)| !auth_set.contains(id))
        .map(|(symbol_id, file_id, name, scope_path)| {
            serde_json::json!({
                "symbol_id": symbol_id, "file_id": file_id, "name": name, "scope_path": scope_path,
            })
        })
        .collect();
    json_result(&json!({
        "project": params.project,
        "matches": findings,
        "http_handler_symbols": http_handler_symbols,
        "guidance": "Routes with mutating verbs (POST/PUT/DELETE/PATCH) in files lacking visible auth middleware are review candidates. A complete check requires per-route middleware-stack inspection beyond regex."
    }))
}

#[cfg(test)]
mod tests {
    use regex::Regex;

    /// The route regex is built inline in the tool body; this mirror keeps the
    /// pattern under test so a malformed alternation fails CI rather than
    /// panicking a worker (the tool uses `.expect("route regex")`).
    fn route_re() -> Regex {
        Regex::new(
            r#"(?m)(\.route\([^)]*post\(|\.route\([^)]*put\(|\.route\([^)]*delete\(|\.route\([^)]*patch\(|app\.(post|put|delete|patch)\(|@(?:Post|Put|Delete|Patch)Mapping|methods=\[?['\x22](POST|PUT|DELETE|PATCH)|\(\s*(?:GET|POST|PUT|DELETE|PATCH|ANY)\s+["]|\(\s*(?:defroutes|context)\s)"#,
        )
        .expect("route regex compiles")
    }

    #[test]
    fn matches_compojure_verbs() {
        let re = route_re();
        assert!(re.is_match(r#"(POST "/users" req (create-user req))"#));
        assert!(re.is_match(r#"(GET "/users/:id" [id] (get-user id))"#));
        assert!(re.is_match(r#"(DELETE "/users/:id" [id] (delete-user id))"#));
        assert!(re.is_match(r#"(defroutes app-routes (GET "/" [] "ok"))"#));
        assert!(re.is_match(r#"(context "/api" [] routes)"#));
    }

    #[test]
    fn does_not_match_plain_words() {
        let re = route_re();
        // A bare symbol named `post` or the word GET in prose must not match.
        assert!(!re.is_match("(let [post 3] post)"));
        assert!(!re.is_match(";; GET the value from the map"));
    }

    #[test]
    fn still_matches_axum_and_spring() {
        let re = route_re();
        assert!(re.is_match(r#".route("/x", post(handler))"#));
        assert!(re.is_match("@PostMapping(\"/x\")"));
    }
}
