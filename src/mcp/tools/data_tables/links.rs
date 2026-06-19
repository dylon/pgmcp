//! Data-table ⇄ experiment/work-item link tools (ADR-023, v44).
//!
//! `data_table_link` / `data_table_unlink` over the `data_table_links` bridge.
//! The target's existence is verified (the bridge is polymorphic, so it carries
//! no DB-level FK to experiments/work_items) to avoid dangling links.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use super::{db_err, invalid, resolve_table};
use crate::context::SystemContext;
use crate::datatable::link_target::LinkTargetType;
use crate::db::queries;
use crate::mcp::server::{DataTableLinkParams, DataTableUnlinkParams};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

async fn target_exists(
    pool: &sqlx::PgPool,
    target: LinkTargetType,
    id: i64,
) -> Result<bool, McpError> {
    let sql = match target {
        LinkTargetType::Experiment => {
            "SELECT EXISTS(SELECT 1 FROM experiments WHERE id = $1 AND valid_to IS NULL)"
        }
        LinkTargetType::WorkItem => "SELECT EXISTS(SELECT 1 FROM work_items WHERE id = $1)",
    };
    sqlx::query_scalar::<_, bool>(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .map_err(db_err)
}

pub async fn tool_data_table_link(
    ctx: &SystemContext,
    params: DataTableLinkParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let target = LinkTargetType::parse(&params.target_type)
        .ok_or_else(|| invalid("target_type must be experiment|work_item"))?;
    let table = resolve_table(
        pool,
        params.table.as_deref(),
        params.table_id,
        params.project.as_deref(),
    )
    .await?;
    if !target_exists(pool, target, params.target_id).await? {
        return Err(invalid(format!(
            "no {} with id {}",
            target.as_str(),
            params.target_id
        )));
    }
    let link_id = queries::link_data_table(
        pool,
        table.id,
        target.as_str(),
        params.target_id,
        params.role.as_deref(),
    )
    .await
    .map_err(db_err)?;
    json_result(&json!({
        "link_id": link_id,
        "table_id": table.id,
        "table": table.name,
        "target_type": target.as_str(),
        "target_id": params.target_id,
        "role": params.role,
    }))
}

pub async fn tool_data_table_unlink(
    ctx: &SystemContext,
    params: DataTableUnlinkParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let target = LinkTargetType::parse(&params.target_type)
        .ok_or_else(|| invalid("target_type must be experiment|work_item"))?;
    let table = resolve_table(
        pool,
        params.table.as_deref(),
        params.table_id,
        params.project.as_deref(),
    )
    .await?;
    let removed = queries::unlink_data_table(pool, table.id, target.as_str(), params.target_id)
        .await
        .map_err(db_err)?;
    json_result(&json!({
        "table_id": table.id,
        "target_type": target.as_str(),
        "target_id": params.target_id,
        "removed": removed,
    }))
}
