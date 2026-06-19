//! `categorical_lint` — verify the strict (extensive-sum) composition laws of
//! the Containment functor over the hierarchical rollup (ADR-028, item 4). The
//! workspace total of each extensive metric must equal the sum over projects;
//! a mismatch is a real data-integrity bug, not a modeling choice.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::category::STRICT_LAWS;
use crate::context::SystemContext;
use crate::mcp::server::CategoricalLintParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_categorical_lint(
    ctx: &SystemContext,
    params: CategoricalLintParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    if params.rebuild.unwrap_or(false) {
        crate::hierarchy::rollup::persist_group_workspace_rollup(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("rollup: {e}"), None))?;
    }

    let mut violations = Vec::new();
    for law in STRICT_LAWS {
        // `law.column` is from a compile-time const list, never user input — safe
        // to interpolate. Compare the workspace row to the sum over projects.
        let sql = format!(
            "SELECT COALESCE((SELECT {col} FROM hier_group_metrics WHERE level = 'workspace' LIMIT 1), 0)::int8,
                    COALESCE((SELECT SUM({col}) FROM project_metrics), 0)::int8",
            col = law.column
        );
        let (workspace, sum_projects): (i64, i64) =
            sqlx::query_as(sqlx::AssertSqlSafe(sql.as_str()))
                .fetch_one(pool)
                .await
                .map_err(|e| McpError::internal_error(format!("law {}: {e}", law.name), None))?;
        if workspace != sum_projects {
            violations.push(json!({
                "law": law.name,
                "column": law.column,
                "workspace": workspace,
                "sum_of_projects": sum_projects,
                "delta": workspace - sum_projects,
            }));
        }
    }

    json_result(&json!({
        "laws_checked": STRICT_LAWS.len(),
        "ok": violations.is_empty(),
        "violations": violations,
        "note": "Strict extensive-sum laws of the Containment functor (symbol⊳…⊳workspace). \
    A violation means the rollup lost or double-counted an extensive metric — a data-integrity bug. \
    Run workspace_architecture_quality(rebuild=true) or the graph-analysis cron if the rollup is stale.",
    }))
}
