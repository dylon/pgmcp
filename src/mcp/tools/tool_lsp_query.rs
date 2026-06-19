//! `lsp_query` — read-only LSP-shaped analytical queries over the indexed symbol
//! graph (ADR-026). One tool, dispatched on a closed `LspOp` vocab. No mutating
//! operations are exposed (analysis only). Empty results carry `guidance` rather
//! than erroring, so an agent learns what data would light an op up.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::{LspOp, LspQueryParams};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

fn bad(msg: &str) -> McpError {
    McpError::invalid_params(msg.to_string(), None)
}

/// Resolve a file path to its id within `project_id` (exact relative/abs, then suffix).
async fn file_in_project(
    pool: &sqlx::PgPool,
    project_id: i32,
    path: &str,
) -> Result<Option<i64>, McpError> {
    sqlx::query_scalar::<_, i64>(
        "SELECT id FROM indexed_files
          WHERE project_id = $1
            AND (relative_path = $2 OR path = $2 OR relative_path LIKE '%' || $2)
          ORDER BY length(relative_path) ASC
          LIMIT 1",
    )
    .bind(project_id)
    .bind(path)
    .fetch_optional(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("file lookup: {e}"), None))
}

/// First defining `file_symbols.id` of `name` in the project (for hover / scope /
/// call-hierarchy anchoring).
async fn symbol_id_of(
    pool: &sqlx::PgPool,
    project_id: i32,
    name: &str,
) -> Result<Option<i64>, McpError> {
    sqlx::query_scalar::<_, i64>(
        "SELECT s.id FROM file_symbols s JOIN indexed_files f ON f.id = s.file_id
          WHERE f.project_id = $1 AND s.name = $2
          ORDER BY s.id LIMIT 1",
    )
    .bind(project_id)
    .bind(name)
    .fetch_optional(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("symbol lookup: {e}"), None))
}

type SymRow = (
    String,
    String,
    i32,
    i32,
    Option<String>,
    Option<String>,
    String,
);

/// Symbols of one file: (name, kind, start_line, end_line, visibility, signature, path).
async fn symbols_for_file(pool: &sqlx::PgPool, file_id: i64) -> Result<Vec<SymRow>, McpError> {
    sqlx::query_as::<_, SymRow>(
        "SELECT s.name, s.kind, s.start_line, s.end_line, s.visibility, s.signature, f.relative_path
           FROM file_symbols s JOIN indexed_files f ON f.id = s.file_id
          WHERE s.file_id = $1 ORDER BY s.start_line",
    )
    .bind(file_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("symbols_for_file: {e}"), None))
}

fn sym_json(r: &SymRow) -> serde_json::Value {
    json!({
        "name": r.0, "kind": r.1, "start_line": r.2, "end_line": r.3,
        "visibility": r.4, "signature": r.5, "path": r.6,
    })
}

pub async fn tool_lsp_query(
    ctx: &SystemContext,
    params: LspQueryParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let op = LspOp::parse(&params.op)
        .ok_or_else(|| bad("invalid op (see capabilities for the full list)"))?;
    let limit = params.limit.unwrap_or(100).clamp(1, 1000);

    if op == LspOp::Capabilities {
        return json_result(&json!({
            "ops": LspOp::ALL.iter().map(|o| o.as_str()).collect::<Vec<_>>(),
            "read_only": true,
            "backing": {
                "file_symbols": "document_symbol, workspace_symbol, definition, hover, signature_help, folding_range",
                "symbol_occurrences": "references, document_highlight (scope-aware via enclosing_symbol_id)",
                "symbol_references": "call_hierarchy_*, type_hierarchy_*, implementation",
            },
            "note": "Analysis only — no rename/format/code-action. Coverage tracks extracted shadow-ASR data.",
        }));
    }

    let project_id = project_id_or_err(ctx, &params.project).await?;
    let sym = || {
        params
            .symbol
            .clone()
            .ok_or_else(|| bad("this op requires `symbol`"))
    };
    let path = || {
        params
            .file_path
            .clone()
            .ok_or_else(|| bad("this op requires `file_path`"))
    };

    match op {
        LspOp::Capabilities => unreachable!("handled above"),

        LspOp::DocumentSymbol | LspOp::FoldingRange => {
            let fp = path()?;
            let file_id = file_in_project(pool, project_id, &fp)
                .await?
                .ok_or_else(|| bad(&format!("no file '{fp}' in project")))?;
            let rows = symbols_for_file(pool, file_id).await?;
            if op == LspOp::FoldingRange {
                let ranges: Vec<_> = rows
                    .iter()
                    .map(|r| json!({"name": r.0, "kind": r.1, "start_line": r.2, "end_line": r.3}))
                    .collect();
                json_result(
                    &json!({"op": op.as_str(), "file": fp, "count": ranges.len(), "ranges": ranges}),
                )
            } else {
                let syms: Vec<_> = rows.iter().map(sym_json).collect();
                json_result(
                    &json!({"op": op.as_str(), "file": fp, "count": syms.len(), "symbols": syms}),
                )
            }
        }

        LspOp::WorkspaceSymbol => {
            let q = sym()?;
            let rows = sqlx::query_as::<_, SymRow>(
                "SELECT s.name, s.kind, s.start_line, s.end_line, s.visibility, s.signature, f.relative_path
                   FROM file_symbols s JOIN indexed_files f ON f.id = s.file_id
                  WHERE f.project_id = $1 AND s.name ILIKE '%' || $2 || '%'
                  ORDER BY (s.name = $2) DESC, s.name LIMIT $3",
            )
            .bind(project_id).bind(&q).bind(limit)
            .fetch_all(pool).await
            .map_err(|e| McpError::internal_error(format!("workspace_symbol: {e}"), None))?;
            let syms: Vec<_> = rows.iter().map(sym_json).collect();
            json_result(
                &json!({"op": op.as_str(), "query": q, "count": syms.len(), "symbols": syms}),
            )
        }

        LspOp::Definition => {
            let name = sym()?;
            let rows = sqlx::query_as::<_, SymRow>(
                "SELECT s.name, s.kind, s.start_line, s.end_line, s.visibility, s.signature, f.relative_path
                   FROM file_symbols s JOIN indexed_files f ON f.id = s.file_id
                  WHERE f.project_id = $1 AND s.name = $2 ORDER BY s.id LIMIT $3",
            )
            .bind(project_id).bind(&name).bind(limit)
            .fetch_all(pool).await
            .map_err(|e| McpError::internal_error(format!("definition: {e}"), None))?;
            let defs: Vec<_> = rows.iter().map(sym_json).collect();
            json_result(&json!({
                "op": op.as_str(), "symbol": name, "count": defs.len(), "definitions": defs,
                "guidance": if defs.is_empty() { Some("no definition found; try workspace_symbol for a fuzzy match") } else { None },
            }))
        }

        LspOp::References | LspOp::DocumentHighlight => {
            let name = sym()?;
            let rows = if op == LspOp::DocumentHighlight {
                let fp = path()?;
                let file_id = file_in_project(pool, project_id, &fp)
                    .await?
                    .ok_or_else(|| bad(&format!("no file '{fp}' in project")))?;
                queries::occurrences_of_name(pool, project_id, &name, Some(file_id), limit).await
            } else if let Some(scope_name) = &params.scope {
                match symbol_id_of(pool, project_id, scope_name).await? {
                    Some(sid) => queries::occurrences_in_scope(pool, sid, Some(&name), limit).await,
                    None => return Err(bad(&format!("scope symbol '{scope_name}' not found"))),
                }
            } else {
                queries::occurrences_of_name(pool, project_id, &name, None, limit).await
            }
            .map_err(|e| McpError::internal_error(format!("occurrences: {e}"), None))?;
            json_result(&json!({
                "op": op.as_str(), "symbol": name, "count": rows.len(), "references": rows,
                "guidance": if rows.is_empty() { Some("no occurrences indexed yet; the symbol-extraction cron populates symbol_occurrences") } else { None },
            }))
        }

        LspOp::Hover | LspOp::SignatureHelp => {
            let name = sym()?;
            let head = sqlx::query_as::<_, (i64, String, String, Option<String>, Option<String>)>(
                "SELECT s.id, s.name, s.kind, s.signature, s.scope_path
                   FROM file_symbols s JOIN indexed_files f ON f.id = s.file_id
                  WHERE f.project_id = $1 AND s.name = $2 ORDER BY s.id LIMIT 1",
            )
            .bind(project_id)
            .bind(&name)
            .fetch_optional(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("hover: {e}"), None))?;
            let Some((sid, sname, kind, signature, scope_path)) = head else {
                return json_result(&json!({"op": op.as_str(), "symbol": name, "found": false,
                    "guidance": "no such symbol; try workspace_symbol"}));
            };
            let params_rows = sqlx::query_as::<_, (String, Option<String>)>(
                "SELECT name, type_raw FROM symbol_parameters WHERE symbol_id = $1 ORDER BY position",
            )
            .bind(sid).fetch_all(pool).await.unwrap_or_default();
            let effects = sqlx::query_scalar::<_, String>(
                "SELECT effect FROM symbol_effects WHERE symbol_id = $1 ORDER BY effect",
            )
            .bind(sid)
            .fetch_all(pool)
            .await
            .unwrap_or_default();
            json_result(&json!({
                "op": op.as_str(), "symbol": sname, "kind": kind, "found": true,
                "signature": signature, "scope_path": scope_path,
                "parameters": params_rows.iter().map(|(n, t)| json!({"name": n, "type": t})).collect::<Vec<_>>(),
                "effects": effects,
            }))
        }

        LspOp::CallHierarchyOutgoing => {
            let name = sym()?;
            let Some(sid) = symbol_id_of(pool, project_id, &name).await? else {
                return json_result(
                    &json!({"op": op.as_str(), "symbol": name, "count": 0, "callees": [],
                    "guidance": "symbol not found"}),
                );
            };
            let rows = sqlx::query_as::<_, (String, String, i32)>(
                "SELECT target_raw, ref_kind, source_line FROM symbol_references
                  WHERE source_symbol_id = $1 AND ref_kind IN ('call','method_call')
                  ORDER BY source_line LIMIT $2",
            )
            .bind(sid)
            .bind(limit)
            .fetch_all(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("outgoing: {e}"), None))?;
            json_result(
                &json!({"op": op.as_str(), "symbol": name, "count": rows.len(),
                "callees": rows.iter().map(|(t, k, l)| json!({"target": t, "ref_kind": k, "line": l})).collect::<Vec<_>>()}),
            )
        }

        LspOp::CallHierarchyIncoming => {
            let name = sym()?;
            let rows = sqlx::query_as::<_, (Option<String>, String, i32, String)>(
                "SELECT srcs.name, r.ref_kind, r.source_line, f.relative_path
                   FROM symbol_references r
                   JOIN indexed_files f ON f.id = r.source_file_id
                   LEFT JOIN file_symbols srcs ON srcs.id = r.source_symbol_id
                  WHERE f.project_id = $1 AND r.target_raw = $2 AND r.ref_kind IN ('call','method_call')
                  ORDER BY f.relative_path, r.source_line LIMIT $3",
            )
            .bind(project_id).bind(&name).bind(limit).fetch_all(pool).await
            .map_err(|e| McpError::internal_error(format!("incoming: {e}"), None))?;
            json_result(
                &json!({"op": op.as_str(), "symbol": name, "count": rows.len(),
                "callers": rows.iter().map(|(s, k, l, p)| json!({"caller": s, "ref_kind": k, "line": l, "path": p})).collect::<Vec<_>>()}),
            )
        }

        LspOp::Implementation | LspOp::TypeHierarchySub | LspOp::TypeHierarchySuper => {
            // Best-effort over typed reference edges. ref_kind vocabulary may not
            // carry implements/extends for every backend → empty + guidance.
            let name = sym()?;
            let kinds: &[&str] = match op {
                LspOp::TypeHierarchySuper => &["extends", "implements", "inherits"],
                _ => &["extends", "implements", "inherits", "type_use"],
            };
            let rows = sqlx::query_as::<_, (Option<String>, String, i32, String)>(
                "SELECT srcs.name, r.ref_kind, r.source_line, f.relative_path
                   FROM symbol_references r
                   JOIN indexed_files f ON f.id = r.source_file_id
                   LEFT JOIN file_symbols srcs ON srcs.id = r.source_symbol_id
                  WHERE f.project_id = $1 AND r.target_raw = $2 AND r.ref_kind = ANY($3)
                  ORDER BY f.relative_path, r.source_line LIMIT $4",
            )
            .bind(project_id)
            .bind(&name)
            .bind(kinds)
            .bind(limit)
            .fetch_all(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("hierarchy: {e}"), None))?;
            json_result(&json!({
                "op": op.as_str(), "symbol": name, "count": rows.len(),
                "results": rows.iter().map(|(s, k, l, p)| json!({"name": s, "ref_kind": k, "line": l, "path": p})).collect::<Vec<_>>(),
                "guidance": if rows.is_empty() { Some("no typed inheritance/impl edges indexed for this symbol's language") } else { None },
            }))
        }

        LspOp::TypeDefinition => {
            // The declared type(s) of `symbol`, resolved to their definitions.
            let name = sym()?;
            let Some(sid) = symbol_id_of(pool, project_id, &name).await? else {
                return json_result(
                    &json!({"op": op.as_str(), "symbol": name, "count": 0, "type_definitions": [],
                    "guidance": "symbol not found"}),
                );
            };
            let type_names = sqlx::query_scalar::<_, String>(
                "SELECT DISTINCT unnest(type_tags) FROM symbol_parameters WHERE symbol_id = $1 AND type_tags <> '{}'",
            )
            .bind(sid).fetch_all(pool).await.unwrap_or_default();
            let mut defs = Vec::new();
            for tn in &type_names {
                let rows = sqlx::query_as::<_, SymRow>(
                    "SELECT s.name, s.kind, s.start_line, s.end_line, s.visibility, s.signature, f.relative_path
                       FROM file_symbols s JOIN indexed_files f ON f.id = s.file_id
                      WHERE f.project_id = $1 AND s.name = $2 AND s.kind IN ('struct','enum','class','trait','interface','type')
                      LIMIT 3",
                )
                .bind(project_id).bind(tn).fetch_all(pool).await.unwrap_or_default();
                for r in &rows {
                    defs.push(sym_json(r));
                }
            }
            json_result(&json!({
                "op": op.as_str(), "symbol": name, "declared_types": type_names, "count": defs.len(), "type_definitions": defs,
                "guidance": if defs.is_empty() { Some("no resolvable declared type; use hover for raw type text, then definition on the type name") } else { None },
            }))
        }
    }
}
