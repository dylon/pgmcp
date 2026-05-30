//! Analysis & report tool bodies: aggregate (group-by + descriptive metrics)
//! and report (render a table + optional summary to one of seven formats,
//! optionally writing it to a file).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

use super::{
    db_err, invalid, load_schema, parse_filter, parse_sort, resolve_table, validate_field_name,
};
use crate::context::SystemContext;
use crate::datatable::aggregate::{AggFunc, MetricSpec, compute_aggregation};
use crate::datatable::column_type::ColumnType;
use crate::datatable::report::{
    CellType, ColumnView, DataReportFormat, TableReport, cells_from_rows, render,
};
use crate::datatable::validate::ColumnSpec;
use crate::db::queries::{self, RowSort};
use crate::mcp::server::{AggSpecParam, DataTableAggregateParams, DataTableReportParams};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

/// Parse + validate the requested aggregations against the declared column types
/// (rejecting numeric-only functions on declared non-numeric columns).
fn parse_metrics(
    types: &HashMap<String, ColumnType>,
    aggregations: &[AggSpecParam],
) -> Result<Vec<MetricSpec>, McpError> {
    let mut out = Vec::with_capacity(aggregations.len());
    for a in aggregations {
        let func = AggFunc::parse(&a.func)
            .ok_or_else(|| invalid(format!("unknown aggregation func '{}'", a.func)))?;
        let field = a.field.clone().filter(|f| !f.trim().is_empty());
        if func.needs_field() && field.is_none() {
            return Err(invalid(format!("func '{}' requires a field", a.func)));
        }
        if let Some(f) = &field {
            validate_field_name(f)?;
            if func.requires_numeric()
                && let Some(ty) = types.get(f)
                && !ty.is_numeric()
            {
                return Err(invalid(format!(
                    "func '{}' requires a numeric column; '{f}' is declared {}",
                    a.func,
                    ty.as_str()
                )));
            }
        }
        let alias = a
            .alias
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| func.default_alias(field.as_deref()));
        out.push(MetricSpec { func, field, alias });
    }
    Ok(out)
}

pub async fn tool_data_table_aggregate(
    ctx: &SystemContext,
    params: DataTableAggregateParams,
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
    let group_by = params.group_by.clone().unwrap_or_default();
    for g in &group_by {
        validate_field_name(g)?;
    }
    if params.aggregations.is_empty() {
        return Err(invalid("`aggregations` must be non-empty"));
    }
    let metrics = parse_metrics(&types, &params.aggregations)?;

    let rows = queries::select_rows(
        pool,
        t.id,
        &filter,
        &RowSort::default(),
        queries::MAX_AGG_SCAN,
        0,
        &types,
    )
    .await
    .map_err(db_err)?;
    let data: Vec<Value> = rows.into_iter().map(|r| r.data).collect();
    let result = compute_aggregation(&data, &group_by, &metrics, &types);

    let mut v = serde_json::to_value(&result)
        .map_err(|e| McpError::internal_error(format!("serialize aggregation: {e}"), None))?;
    if let Value::Object(ref mut m) = v {
        m.insert("table".into(), json!(t.name));
    }
    json_result(&v)
}

/// Resolve the report's column projection: explicit `columns`, else the declared
/// schema order, else the union of keys across the rendered rows (open tables).
fn build_columns(
    projection: &Option<Vec<String>>,
    specs: &[ColumnSpec],
    data: &[Value],
) -> Vec<ColumnView> {
    let infer = |name: &str| -> CellType {
        for row in data {
            if let Some(v) = row.get(name)
                && !v.is_null()
            {
                return CellType::infer(v);
            }
        }
        CellType::Text
    };
    if let Some(cols) = projection {
        return cols
            .iter()
            .map(|name| {
                let ty = specs
                    .iter()
                    .find(|s| &s.name == name)
                    .map(|s| CellType::from_column(s.data_type))
                    .unwrap_or_else(|| infer(name));
                ColumnView::new(name.clone(), ty)
            })
            .collect();
    }
    if !specs.is_empty() {
        return specs
            .iter()
            .map(|s| ColumnView::new(s.name.clone(), CellType::from_column(s.data_type)))
            .collect();
    }
    let mut seen = Vec::new();
    let mut set = HashSet::new();
    for row in data {
        if let Some(obj) = row.as_object() {
            for k in obj.keys() {
                if set.insert(k.clone()) {
                    seen.push(k.clone());
                }
            }
        }
    }
    seen.into_iter()
        .map(|name| {
            let ty = infer(&name);
            ColumnView::new(name, ty)
        })
        .collect()
}

fn fmt_name(f: DataReportFormat) -> &'static str {
    match f {
        DataReportFormat::Markdown => "markdown",
        DataReportFormat::Org => "org",
        DataReportFormat::Latex => "latex",
        DataReportFormat::Html => "html",
        DataReportFormat::Text => "text",
        DataReportFormat::Json => "json",
        DataReportFormat::Csv => "csv",
    }
}

pub async fn tool_data_table_report(
    ctx: &SystemContext,
    params: DataTableReportParams,
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

    let fmt = match params.format.as_deref() {
        None => DataReportFormat::Markdown,
        Some(s) => DataReportFormat::parse(s).ok_or_else(|| {
            invalid(format!(
                "unknown format '{s}'; expected {}",
                DataReportFormat::valid_values()
            ))
        })?,
    };

    let (specs, types) = load_schema(pool, t.id).await?;
    let filter = parse_filter(&params.filter, &params.combine)?;
    let sort = parse_sort(&params.sort_by, &params.sort_dir)?;
    let limit = params
        .limit
        .unwrap_or(100)
        .clamp(1, queries::MAX_SELECT_ROWS);

    let rows = queries::select_rows(pool, t.id, &filter, &sort, limit, 0, &types)
        .await
        .map_err(db_err)?;
    let total = queries::count_rows(pool, t.id, &filter, &types)
        .await
        .map_err(db_err)?;
    let data: Vec<Value> = rows.iter().map(|r| r.data.clone()).collect();

    // Optional summary aggregation over the FULL filtered set (capped), not just
    // the rendered detail subset.
    let summary = match &params.summary {
        None => None,
        Some(sp) => {
            let group_by = sp.group_by.clone().unwrap_or_default();
            for g in &group_by {
                validate_field_name(g)?;
            }
            if sp.aggregations.is_empty() {
                return Err(invalid("summary.aggregations must be non-empty"));
            }
            let metrics = parse_metrics(&types, &sp.aggregations)?;
            let all = queries::select_rows(
                pool,
                t.id,
                &filter,
                &RowSort::default(),
                queries::MAX_AGG_SCAN,
                0,
                &types,
            )
            .await
            .map_err(db_err)?;
            let all_data: Vec<Value> = all.into_iter().map(|r| r.data).collect();
            Some(compute_aggregation(&all_data, &group_by, &metrics, &types))
        }
    };

    let columns = build_columns(&params.columns, &specs, &data);
    let cells = cells_from_rows(&columns, &data);
    let report = TableReport {
        title: params.title.clone().unwrap_or_else(|| t.name.clone()),
        columns,
        rows: cells,
        summary,
        caption: params.caption.clone(),
        generated_at: chrono::Utc::now(),
        total_rows: total,
        truncated: (data.len() as i64) < total,
    };
    let rendered = render(&report, fmt);

    match params.write_to_path.as_deref().map(str::trim) {
        Some(path) if !path.is_empty() => {
            let p = std::path::PathBuf::from(path);
            let existed = p.exists();
            if let Some(parent) = p.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .map_err(|e| McpError::internal_error(format!("create dir: {e}"), None))?;
            }
            std::fs::write(&p, rendered.as_bytes()).map_err(|e| {
                McpError::internal_error(format!("write {}: {e}", p.display()), None)
            })?;
            json_result(&json!({
                "table": t.name,
                "format": fmt_name(fmt),
                "path": p.to_string_lossy(),
                "bytes_written": rendered.len(),
                "overwrote": existed,
            }))
        }
        _ => json_result(&json!({
            "table": t.name,
            "format": fmt_name(fmt),
            "rendered": rendered,
        })),
    }
}
