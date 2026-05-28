//! Dependency collectors: prune-candidate imports, deprecated definitions.

use regex::Regex;
use rmcp::ErrorData as McpError;
use serde_json::json;

use super::truncate_preview;
use crate::context::SystemContext;
use crate::mcp::tools::sota_helpers::pool_or_err;
use crate::quality::findings::{Finding, FindingCategory, Severity};

const DEP: FindingCategory = FindingCategory::Dependency;

/// External imports pulled in by exactly one file — consolidation/prune review.
pub async fn collect_dependency_health(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        target_raw: String,
        importers: i64,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT cge.target_raw, COUNT(DISTINCT cge.source_file_id)::BIGINT AS importers
         FROM code_graph_edges cge
         WHERE cge.project_id = $1 AND cge.edge_type = 'import'
           AND cge.target_file_id IS NULL AND cge.target_raw IS NOT NULL
         GROUP BY cge.target_raw
         HAVING COUNT(DISTINCT cge.source_file_id) = 1",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("dependency_health query failed: {e}"), None))?;

    Ok(rows
        .into_iter()
        .map(|r| {
            Finding::new(
                "dependency_health",
                DEP,
                project_name,
                Severity::Low,
                format!(
                    "`{}` is imported by only one file — prune/consolidate candidate",
                    r.target_raw
                ),
            )
            .with_kind(format!("single_importer:{}", r.target_raw))
            .with_raw(json!({ "dependency": r.target_raw, "importers": r.importers }))
        })
        .collect())
}

/// Definitions annotated deprecated — audit remaining callers.
pub async fn collect_deprecated_but_used(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        relative_path: String,
        content: Option<String>,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT relative_path, content FROM indexed_files WHERE project_id = $1 AND content IS NOT NULL",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("deprecated_but_used query failed: {e}"), None))?;

    let re = Regex::new(r"(#\[deprecated|@Deprecated|@deprecated|@Obsolete)").expect("re");
    let mut out = Vec::new();
    for r in &rows {
        let content = r.content.as_deref().unwrap_or("");
        for (i, line) in content.lines().enumerate() {
            if re.is_match(line) {
                out.push(
                    Finding::new(
                        "deprecated_but_used",
                        DEP,
                        project_name,
                        Severity::Low,
                        format!(
                            "Deprecated definition — audit callers: {}",
                            truncate_preview(line, 80)
                        ),
                    )
                    .at(&r.relative_path, (i + 1) as u32)
                    .with_kind("deprecated_definition")
                    .with_raw(json!({ "file": r.relative_path, "line": i + 1 })),
                );
            }
        }
    }
    Ok(out)
}
