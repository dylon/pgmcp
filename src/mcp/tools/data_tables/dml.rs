//! Row DML tool bodies: insert, select, update, delete. Strict tables validate
//! rows (and update patches) against the declared column types; open tables
//! accept any JSON object. Filters/sorts compile to parameterized SQL.

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

use super::{
    MAX_INSERT_BATCH, MAX_ROW_BYTES, WRITER, db_err, invalid, load_schema, parse_filter,
    parse_sort, resolve_table,
};
use crate::context::SystemContext;
use crate::datatable::validate::{fill_defaults, json_type_name, validate_row, value_matches};
use crate::db::queries;
use crate::mcp::server::{
    DataTableDeleteParams, DataTableInsertParams, DataTableSelectParams, DataTableUpdateParams,
};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_data_table_insert(
    ctx: &SystemContext,
    params: DataTableInsertParams,
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

    if params.rows.is_empty() {
        return Err(invalid("`rows` must be non-empty"));
    }
    if params.rows.len() > MAX_INSERT_BATCH {
        return Err(invalid(format!(
            "too many rows ({}); max {MAX_INSERT_BATCH} per call",
            params.rows.len()
        )));
    }

    let strict = t.schema_mode == "strict";
    let (specs, _types) = load_schema(pool, t.id).await?;

    let mut rows_json = Vec::with_capacity(params.rows.len());
    for (i, mut row) in params.rows.into_iter().enumerate() {
        if !row.is_object() {
            return Err(invalid(format!("row {i} must be a JSON object")));
        }
        if strict {
            if let Value::Object(obj) = &mut row {
                fill_defaults(&specs, obj);
            }
            if let Err(errs) = validate_row(&specs, &row, false) {
                let msg = errs
                    .iter()
                    .map(|e| e.message())
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(invalid(format!("row {i}: {msg}")));
            }
        }
        let s = serde_json::to_string(&row)
            .map_err(|e| McpError::internal_error(format!("serialize row {i}: {e}"), None))?;
        if s.len() > MAX_ROW_BYTES {
            return Err(invalid(format!(
                "row {i} is {} bytes; max {MAX_ROW_BYTES}",
                s.len()
            )));
        }
        rows_json.push(s);
    }

    let ids = queries::insert_rows(
        pool,
        t.id,
        &rows_json,
        Some(WRITER),
        params.source.as_deref(),
    )
    .await
    .map_err(db_err)?;
    json_result(&json!({ "inserted": ids.len(), "ids": ids }))
}

pub async fn tool_data_table_select(
    ctx: &SystemContext,
    params: DataTableSelectParams,
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
    let (_specs, types) = load_schema(pool, t.id).await?;
    let filter = parse_filter(&params.filter, &params.combine)?;
    let sort = parse_sort(&params.sort_by, &params.sort_dir)?;
    let limit = params
        .limit
        .unwrap_or(100)
        .clamp(1, queries::MAX_SELECT_ROWS);
    let offset = params.offset.unwrap_or(0).max(0);

    let rows = queries::select_rows(pool, t.id, &filter, &sort, limit, offset, &types)
        .await
        .map_err(db_err)?;
    let total = queries::count_rows(pool, t.id, &filter, &types)
        .await
        .map_err(db_err)?;
    json_result(&json!({ "returned": rows.len(), "total": total, "rows": rows }))
}

pub async fn tool_data_table_update(
    ctx: &SystemContext,
    params: DataTableUpdateParams,
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

    if !params.patch.is_object() {
        return Err(invalid("`patch` must be a JSON object"));
    }
    let (specs, types) = load_schema(pool, t.id).await?;

    // Strict tables: type-check the patched fields against their declared types.
    if t.schema_mode == "strict"
        && let Value::Object(obj) = &params.patch
    {
        for (k, v) in obj {
            if let Some(spec) = specs.iter().find(|s| &s.name == k)
                && !v.is_null()
                && !value_matches(spec.data_type, v)
            {
                return Err(invalid(format!(
                    "patch field '{k}' expected {}, got {}",
                    spec.data_type.as_str(),
                    json_type_name(v)
                )));
            }
        }
    }

    let patch_json = params.patch.to_string();
    let updated = if let Some(rid) = params.row_id {
        queries::update_row_by_id(pool, t.id, rid, &patch_json)
            .await
            .map_err(db_err)?
    } else {
        let filter = parse_filter(&params.filter, &params.combine)?;
        if filter.is_empty() && !params.all.unwrap_or(false) {
            return Err(invalid(
                "refusing a table-wide update; pass a filter, a row_id, or all=true",
            ));
        }
        queries::update_rows(pool, t.id, &filter, &patch_json, &types)
            .await
            .map_err(db_err)?
    };
    json_result(&json!({ "updated": updated }))
}

pub async fn tool_data_table_delete(
    ctx: &SystemContext,
    params: DataTableDeleteParams,
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
    let (_specs, types) = load_schema(pool, t.id).await?;

    let deleted = if let Some(rid) = params.row_id {
        queries::delete_row_by_id(pool, t.id, rid)
            .await
            .map_err(db_err)?
    } else {
        let filter = parse_filter(&params.filter, &params.combine)?;
        if filter.is_empty() && !params.all.unwrap_or(false) {
            return Err(invalid(
                "refusing a table-wide delete; pass a filter, a row_id, or all=true",
            ));
        }
        queries::delete_rows(pool, t.id, &filter, &types)
            .await
            .map_err(db_err)?
    };
    json_result(&json!({ "deleted": deleted }))
}
