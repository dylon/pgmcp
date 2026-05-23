//! `tool_dead_columns` — Columns defined in SQL DDL but never referenced
//! in code (SOTA Phase 9.2).

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::HashSet;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::DeadColumnsParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

const STOPWORDS: &[&str] = &[
    "id", "name", "type", "kind", "value", "key", "code", "status", "data", "text", "path", "url",
    "size", "count", "version", "level", "time", "date",
];

pub async fn tool_dead_columns(
    ctx: &SystemContext,
    params: DeadColumnsParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "dead_columns", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50);

    // Parse migrations / CREATE TABLE definitions for declared columns.
    let sql_rows: Vec<(String, Option<String>)> = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT relative_path, content
         FROM indexed_files
         WHERE project_id = $1
           AND content IS NOT NULL
           AND (relative_path ~ '\\.sql$' OR relative_path ~ '(/migrations?/|/migrate/|/db/migrate/)')",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("DDL scan failed: {}", e), None))?;

    let column_re = Regex::new(
        r"(?ims)CREATE\s+TABLE[^;]*?\(\s*(?P<cols>[^;]+?)\s*\)\s*;|ADD\s+COLUMN\s+(?P<add>[A-Za-z_][A-Za-z0-9_]*)\s+",
    )
    .expect("col regex");
    let mut columns: HashSet<(String, String)> = HashSet::new(); // (table, column)
    for (path, content) in &sql_rows {
        let Some(c) = content else { continue };
        for cap in column_re.captures_iter(c) {
            if let Some(add) = cap.name("add") {
                let name = add.as_str().to_lowercase();
                columns.insert((path.clone(), name));
            } else if let Some(cols) = cap.name("cols") {
                for col_line in cols.as_str().split(',') {
                    let line = col_line.trim();
                    if let Some(token) = line.split_whitespace().next() {
                        let token = token
                            .trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
                            .to_lowercase();
                        if !token.is_empty() {
                            columns.insert((path.clone(), token));
                        }
                    }
                }
            }
        }
    }
    if columns.is_empty() {
        return json_result(&json!({
            "project": params.project,
            "dead_columns": [],
            "guidance": "No DDL columns parsed — project may not have SQL migrations."
        }));
    }

    // Fetch all source text once and search.
    let src_rows: Vec<(String, Option<String>)> = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT relative_path, content
         FROM indexed_files
         WHERE project_id = $1 AND content IS NOT NULL
           AND relative_path !~ '\\.sql$'
           AND relative_path !~ '(/migrations?/|/migrate/|/db/migrate/)'",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Source scan failed: {}", e), None))?;

    let mut dead: Vec<(String, String)> = Vec::new();
    'outer: for (ddl_path, name) in &columns {
        if STOPWORDS.contains(&name.as_str()) || name.len() < 4 {
            continue;
        }
        // Quick contains check (case-insensitive).
        for (_, src) in &src_rows {
            let Some(s) = src else { continue };
            if s.to_lowercase().contains(name) {
                continue 'outer;
            }
        }
        dead.push((ddl_path.clone(), name.clone()));
        if dead.len() >= limit.max(0) as usize {
            break;
        }
    }
    let out: Vec<_> = dead
        .into_iter()
        .map(|(p, c)| json!({"ddl_file": p, "column": c}))
        .collect();
    // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown
    // for the project. Universal enrichment — every tool benefits from
    // surfacing the effect distribution alongside its primary output.
    // Gracefully degrades to empty when the project lookup or
    // shadow-ASR data isn't populated.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        let project_id_opt: Option<i32> =
            sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
                .bind(&params.project)
                .fetch_optional(pool)
                .await
                .unwrap_or(None);
        match project_id_opt {
            Some(pid) => crate::mcp::tools::sema_helpers::effects::effect_counts(pool, pid)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
                .collect(),
            None => Vec::new(),
        }
    })
    .await;

    json_result(&json!({
        "effect_breakdown": effect_breakdown,
        "project": params.project,
        "dead_columns": out,
        "guidance": "Columns declared in DDL but never referenced in non-SQL source. Skips common English stopwords (id/name/value) and short names to limit false positives. Manual verification recommended for ORM-mapped columns accessed via reflection."
    }))
}
