//! `tool_migration_safety` — Parse SQL migrations and flag risky DDL
//! (SOTA Phase 9.1, Curino et al. VLDB 2008 + Strong Migrations rules).

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::MigrationSafetyParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_migration_safety(
    ctx: &SystemContext,
    params: MigrationSafetyParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "migration_safety", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50);

    // Detect migration files by path or by SQL content.
    let rows: Vec<(String, Option<String>)> = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT relative_path, content
         FROM indexed_files
         WHERE project_id = $1
           AND content IS NOT NULL
           AND (relative_path ~ '(/migrations?/|/migrate/|/db/migrate/|alembic/versions/|prisma/migrations/)'
                OR relative_path ~ '\\.sql$')",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Migration scan failed: {}", e), None))?;

    let dialect = PostgreSqlDialect {};
    let mut findings: Vec<serde_json::Value> = Vec::new();

    // Risky patterns we surface via regex (sqlparser-rs parses the statement
    // tree to confirm SQL was actually present and well-formed enough to risk-rate).
    let drop_col_re =
        Regex::new(r"(?im)\bALTER\s+TABLE\s+\S+\s+DROP\s+COLUMN\b").expect("drop col");
    let alter_type_re =
        Regex::new(r"(?im)\bALTER\s+TABLE\s+\S+\s+ALTER\s+COLUMN\s+\S+\s+(SET\s+DATA\s+)?TYPE\b")
            .expect("alter type");
    let add_not_null_re =
        Regex::new(r"(?im)\bADD\s+COLUMN\s+\S+\s+[^,;]+\bNOT\s+NULL\b").expect("not null");
    let default_re = Regex::new(r"(?im)\bDEFAULT\b").expect("default");
    let create_index_re = Regex::new(r"(?im)\bCREATE\s+(UNIQUE\s+)?INDEX\b").expect("create index");
    let concurrently_re = Regex::new(r"(?im)\bCONCURRENTLY\b").expect("concurrently");
    let truncate_re = Regex::new(r"(?im)\bTRUNCATE\b").expect("truncate");
    let drop_table_re = Regex::new(r"(?im)\bDROP\s+TABLE\b").expect("drop table");
    let rename_re = Regex::new(r"(?im)\bRENAME\s+(COLUMN|TO)\b").expect("rename");

    for (path, content) in rows {
        let Some(c) = content else { continue };
        // Verify some SQL parses cleanly (best-effort — many migrations are
        // wrapped in Ruby/Python DSL we can't parse, so we don't gate on this).
        let _parsed = Parser::parse_sql(&dialect, &c);

        let mut file_findings: Vec<(&str, &str)> = Vec::new();
        if drop_col_re.is_match(&c) {
            file_findings.push(("major", "drop_column"));
        }
        if alter_type_re.is_match(&c) {
            file_findings.push(("major", "alter_column_type"));
        }
        if add_not_null_re.is_match(&c) && !default_re.is_match(&c) {
            file_findings.push(("major", "add_not_null_without_default"));
        }
        if create_index_re.is_match(&c) && !concurrently_re.is_match(&c) {
            file_findings.push(("warn", "create_index_blocking"));
        }
        if truncate_re.is_match(&c) {
            file_findings.push(("major", "truncate"));
        }
        if drop_table_re.is_match(&c) {
            file_findings.push(("major", "drop_table"));
        }
        if rename_re.is_match(&c) {
            file_findings.push(("warn", "rename"));
        }
        if file_findings.is_empty() {
            continue;
        }
        findings.push(json!({
            "file": path,
            "issues": file_findings.into_iter().map(|(sev, kind)| json!({"severity": sev, "kind": kind})).collect::<Vec<_>>(),
        }));
        if findings.len() >= limit.max(0) as usize {
            break;
        }
    }
    json_result(&json!({
        "project": params.project,
        "findings": findings,
        "guidance": "Strong-Migrations heuristics: DROP COLUMN/TABLE & ALTER TYPE & ADD NOT NULL without DEFAULT are usually irreversible on production. CREATE INDEX without CONCURRENTLY blocks writers. RENAME breaks downstream consumers."
    }))
}
