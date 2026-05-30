//! Table-definition tool bodies: create, alter, drop, list, describe. These are
//! metadata operations on the three fixed tables — never dynamic DDL.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use super::{
    DROP_CONFIRM_THRESHOLD, WRITER, db_err, embed_table, invalid, resolve_scope, resolve_table,
};
use crate::context::SystemContext;
use crate::datatable::{ColumnType, validate_identifier};
use crate::db::queries::{self, RowFilter, RowSort};
use crate::mcp::server::{
    ColumnDefParam, DataTableAlterParams, DataTableCreateParams, DataTableDescribeParams,
    DataTableDropParams, DataTableListParams,
};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

/// Validate a column declaration's name + type, returning the parsed type.
fn check_column(c: &ColumnDefParam, seen: &mut HashSet<String>) -> Result<(), McpError> {
    let name = c.name.trim();
    validate_identifier(name).map_err(|e| invalid(format!("column '{}': {e}", c.name)))?;
    if !seen.insert(name.to_string()) {
        return Err(invalid(format!("duplicate column '{name}'")));
    }
    if ColumnType::parse(&c.data_type).is_none() {
        return Err(invalid(format!(
            "column '{name}': unknown type '{}'; expected one of {}",
            c.data_type,
            crate::datatable::column_type::sql_in_list()
        )));
    }
    Ok(())
}

pub async fn tool_data_table_create(
    ctx: &SystemContext,
    params: DataTableCreateParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let name = params.name.trim();
    validate_identifier(name).map_err(invalid)?;
    let scope = resolve_scope(pool, params.project.as_deref()).await?;

    if queries::get_table_by_name(pool, scope, name)
        .await
        .map_err(db_err)?
        .is_some()
    {
        return Err(invalid(format!(
            "a data table named '{name}' already exists in this scope"
        )));
    }

    let col_defs = params.columns.unwrap_or_default();
    let mut seen = HashSet::new();
    for c in &col_defs {
        check_column(c, &mut seen)?;
    }
    let schema_mode = if col_defs.is_empty() {
        "open"
    } else {
        "strict"
    };

    let id = queries::create_table(
        pool,
        scope,
        name,
        params.description.as_deref(),
        schema_mode,
        Some(WRITER),
    )
    .await
    .map_err(db_err)?;

    for (i, c) in col_defs.iter().enumerate() {
        let default_json = c.default.as_ref().map(|v| v.to_string());
        queries::add_column(
            pool,
            id,
            c.name.trim(),
            &c.data_type,
            c.required.unwrap_or(false),
            default_json.as_deref(),
            i as i32,
            c.description.as_deref(),
        )
        .await
        .map_err(db_err)?;
    }

    embed_table(ctx, pool, id, name, params.description.as_deref()).await;

    let table = queries::get_table(pool, id).await.map_err(db_err)?;
    let columns = queries::list_columns(pool, id).await.map_err(db_err)?;
    json_result(&json!({ "table": table, "columns": columns }))
}

pub async fn tool_data_table_alter(
    ctx: &SystemContext,
    params: DataTableAlterParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let t = resolve_table(
        pool,
        params.table.as_deref(),
        params.table_id,
        params.project.as_deref(),
    )
    .await?;

    let mut final_name = t.name.clone();
    let meta_changed = params.rename_to.is_some() || params.description.is_some();

    // Rename the table (validate + scope-uniqueness).
    if let Some(rn) = params.rename_to.as_deref() {
        let rn = rn.trim();
        validate_identifier(rn).map_err(invalid)?;
        if rn != t.name
            && queries::get_table_by_name(pool, t.project_id, rn)
                .await
                .map_err(db_err)?
                .is_some()
        {
            return Err(invalid(format!(
                "a data table named '{rn}' already exists in this scope"
            )));
        }
        final_name = rn.to_string();
    }
    if meta_changed {
        queries::update_table_meta(
            pool,
            t.id,
            params.rename_to.as_deref().map(str::trim),
            params.description.as_deref(),
        )
        .await
        .map_err(db_err)?;
    }

    // Add columns (appended after the current max position).
    let existing = queries::list_columns(pool, t.id).await.map_err(db_err)?;
    let mut names: HashSet<String> = existing.iter().map(|c| c.name.clone()).collect();
    let base_pos = existing.iter().map(|c| c.position).max().unwrap_or(-1) + 1;
    for (i, c) in params.add_columns.unwrap_or_default().iter().enumerate() {
        let cn = c.name.trim();
        validate_identifier(cn).map_err(|e| invalid(format!("column '{}': {e}", c.name)))?;
        if !names.insert(cn.to_string()) {
            return Err(invalid(format!("column '{cn}' already exists")));
        }
        if ColumnType::parse(&c.data_type).is_none() {
            return Err(invalid(format!(
                "column '{cn}': unknown type '{}'",
                c.data_type
            )));
        }
        let default_json = c.default.as_ref().map(|v| v.to_string());
        queries::add_column(
            pool,
            t.id,
            cn,
            &c.data_type,
            c.required.unwrap_or(false),
            default_json.as_deref(),
            base_pos + i as i32,
            c.description.as_deref(),
        )
        .await
        .map_err(db_err)?;
    }

    // Drop columns.
    for dc in params.drop_columns.unwrap_or_default() {
        queries::drop_column(pool, t.id, dc.trim())
            .await
            .map_err(db_err)?;
    }

    // Modify columns (required/default, then rename + JSON-key migrate).
    for mc in params.modify_columns.unwrap_or_default() {
        let cn = mc.name.trim();
        if mc.required.is_some() || mc.default.is_some() {
            let default_json = mc.default.as_ref().map(|v| v.to_string());
            queries::update_column(
                pool,
                t.id,
                cn,
                mc.required,
                mc.default.is_some(),
                default_json.as_deref(),
            )
            .await
            .map_err(db_err)?;
        }
        if let Some(rt) = mc.rename_to.as_deref() {
            let rt = rt.trim();
            validate_identifier(rt).map_err(|e| invalid(format!("rename '{cn}'->'{rt}': {e}")))?;
            queries::rename_column(pool, t.id, cn, rt)
                .await
                .map_err(db_err)?;
            queries::rename_row_key(pool, t.id, cn, rt)
                .await
                .map_err(db_err)?;
        }
    }

    // Reconcile schema_mode against the post-edit column set.
    let final_cols = queries::list_columns(pool, t.id).await.map_err(db_err)?;
    let want_mode = if final_cols.is_empty() {
        "open"
    } else {
        "strict"
    };
    if want_mode != t.schema_mode {
        queries::set_schema_mode(pool, t.id, want_mode)
            .await
            .map_err(db_err)?;
    }

    if meta_changed {
        let desc = params.description.as_deref().or(t.description.as_deref());
        embed_table(ctx, pool, t.id, &final_name, desc).await;
    }

    let table = queries::get_table(pool, t.id).await.map_err(db_err)?;
    json_result(&json!({ "table": table, "columns": final_cols }))
}

pub async fn tool_data_table_drop(
    ctx: &SystemContext,
    params: DataTableDropParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let t = resolve_table(
        pool,
        params.table.as_deref(),
        params.table_id,
        params.project.as_deref(),
    )
    .await?;

    let n = queries::count_rows(pool, t.id, &RowFilter::none(), &HashMap::new())
        .await
        .map_err(db_err)?;
    if n > DROP_CONFIRM_THRESHOLD && !params.confirm.unwrap_or(false) {
        return Err(invalid(format!(
            "table '{}' holds {n} rows; pass confirm=true to drop it and all its data",
            t.name
        )));
    }
    queries::delete_table(pool, t.id).await.map_err(db_err)?;
    json_result(&json!({ "dropped": true, "table": t.name, "rows_deleted": n }))
}

pub async fn tool_data_table_list(
    ctx: &SystemContext,
    params: DataTableListParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let scope = resolve_scope(pool, params.project.as_deref()).await?;
    let limit = params.limit.unwrap_or(100).clamp(1, 1000);
    let tables = queries::list_tables(pool, scope, limit)
        .await
        .map_err(db_err)?;
    json_result(&json!({ "count": tables.len(), "tables": tables }))
}

pub async fn tool_data_table_describe(
    ctx: &SystemContext,
    params: DataTableDescribeParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let t = resolve_table(
        pool,
        params.table.as_deref(),
        params.table_id,
        params.project.as_deref(),
    )
    .await?;
    let columns = queries::list_columns(pool, t.id).await.map_err(db_err)?;
    let row_count = queries::count_rows(pool, t.id, &RowFilter::none(), &HashMap::new())
        .await
        .map_err(db_err)?;
    let sample_n = params.sample.unwrap_or(5).clamp(0, 50);
    let sample_rows = if sample_n > 0 {
        queries::select_rows(
            pool,
            t.id,
            &RowFilter::none(),
            &RowSort::default(),
            sample_n,
            0,
            &HashMap::new(),
        )
        .await
        .map_err(db_err)?
    } else {
        Vec::new()
    };
    json_result(&json!({
        "table": t,
        "columns": columns,
        "row_count": row_count,
        "sample_rows": sample_rows,
    }))
}
