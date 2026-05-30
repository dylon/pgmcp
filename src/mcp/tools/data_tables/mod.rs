//! MCP tool bodies for JSON data tables (domain in `crate::datatable`, queries
//! in `crate::db::queries::data_tables`, schema in the v19 migration).
//!
//! Agent-facing CRUD + analysis + discovery. The safety boundary is
//! *structural*: there is no dynamic DDL — every operation is an INSERT /
//! SELECT / UPDATE / DELETE against the three fixed tables, and all user values
//! are bound parameters (see `crate::db::queries::data_tables`). Each tool is a
//! `pub async fn tool_<name>(ctx: &SystemContext, params: <Name>Params) ->
//! Result<CallToolResult, McpError>`; the `#[tool]` method on `McpServer`
//! forwards into it, mirroring `tool_experiments`.

#![allow(unused_imports)]

mod analysis;
mod ddl;
mod dml;
mod search;

pub use analysis::*;
pub use ddl::*;
pub use dml::*;
pub use search::*;

use std::collections::HashMap;

use rmcp::ErrorData as McpError;
use sqlx::PgPool;

use crate::context::SystemContext;
use crate::datatable::validate::ColumnSpec;
use crate::datatable::{ColumnType, Combinator, FilterOp, SortDir};
use crate::db::queries::{self, DataTableRow, FieldPredicate, RowFilter, RowSort};
use crate::mcp::server::FilterClauseParam;

/// Recorded author for agent-written tables/rows (these tools are agent-facing;
/// the row-level `source` param carries any caller-supplied provenance).
pub(crate) const WRITER: &str = "agent";

/// Signature stamped on table embeddings; bump if the embed prose changes so the
/// migration cron re-embeds existing installs.
pub(crate) const EMBED_SIGNATURE: &str = "data-table-v1";

/// Max rows accepted by a single `data_table_insert`.
pub(crate) const MAX_INSERT_BATCH: usize = 1000;
/// Max serialized size of a single row's `data` (256 KiB).
pub(crate) const MAX_ROW_BYTES: usize = 256 * 1024;
/// Row-count threshold above which `data_table_drop` requires `confirm = true`.
pub(crate) const DROP_CONFIRM_THRESHOLD: i64 = 50;

pub(crate) fn invalid(msg: impl Into<String>) -> McpError {
    McpError::invalid_params(msg.into(), None)
}

pub(crate) fn db_err(e: sqlx::Error) -> McpError {
    McpError::internal_error(format!("db error: {e}"), None)
}

/// Resolve an optional project name to its id, erroring if a name was supplied
/// but no such project exists (avoids silently falling back to global scope).
pub(crate) async fn resolve_scope(
    pool: &PgPool,
    project: Option<&str>,
) -> Result<Option<i32>, McpError> {
    match project {
        None => Ok(None),
        Some(name) => queries::resolve_project_id(pool, Some(name))
            .await
            .map_err(db_err)?
            .map(Some)
            .ok_or_else(|| invalid(format!("project '{name}' not found"))),
    }
}

/// Resolve a table by `table_id` or by `table` name within `project` scope.
pub(crate) async fn resolve_table(
    pool: &PgPool,
    table: Option<&str>,
    table_id: Option<i64>,
    project: Option<&str>,
) -> Result<DataTableRow, McpError> {
    if let Some(id) = table_id {
        return queries::get_table(pool, id)
            .await
            .map_err(db_err)?
            .ok_or_else(|| invalid(format!("no data table with id {id}")));
    }
    let name = table
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("provide `table` (name) or `table_id`"))?;
    let scope = resolve_scope(pool, project).await?;
    queries::get_table_by_name(pool, scope, name)
        .await
        .map_err(db_err)?
        .ok_or_else(|| invalid(format!("no data table '{name}' in this scope")))
}

/// Load a table's declared columns as validation specs + a name→type lookup for
/// the filter/sort/aggregate SQL casts. Empty for an open (free-form) table.
pub(crate) async fn load_schema(
    pool: &PgPool,
    table_id: i64,
) -> Result<(Vec<ColumnSpec>, HashMap<String, ColumnType>), McpError> {
    let cols = queries::list_columns(pool, table_id)
        .await
        .map_err(db_err)?;
    let mut specs = Vec::with_capacity(cols.len());
    let mut types = HashMap::with_capacity(cols.len());
    for c in &cols {
        let ty = ColumnType::parse(&c.data_type).unwrap_or(ColumnType::Json);
        types.insert(c.name.clone(), ty);
        let default = c
            .default_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
        specs.push(ColumnSpec {
            name: c.name.clone(),
            data_type: ty,
            required: c.required,
            default,
        });
    }
    Ok((specs, types))
}

/// A JSON field key is inlined into SQL as a `''`-escaped string literal, so any
/// value is already injection-safe; we only reject the empty key for clarity.
pub(crate) fn validate_field_name(name: &str) -> Result<(), McpError> {
    if name.trim().is_empty() {
        Err(invalid("field name must be non-empty"))
    } else {
        Ok(())
    }
}

/// Build a [`RowFilter`] from the param clauses + combinator string.
pub(crate) fn parse_filter(
    clauses: &Option<Vec<FilterClauseParam>>,
    combine: &Option<String>,
) -> Result<RowFilter, McpError> {
    let combinator = match combine.as_deref() {
        None => Combinator::All,
        Some(s) => Combinator::parse(s).ok_or_else(|| invalid("`combine` must be all|any"))?,
    };
    let mut predicates = Vec::new();
    if let Some(cs) = clauses {
        for c in cs {
            validate_field_name(&c.field)?;
            let op = FilterOp::parse(&c.op)
                .ok_or_else(|| invalid(format!("unknown filter op '{}'", c.op)))?;
            if op.needs_value() && c.value.is_none() {
                return Err(invalid(format!("filter op '{}' requires a value", c.op)));
            }
            predicates.push(FieldPredicate {
                field: c.field.clone(),
                op,
                value: c.value.clone().unwrap_or(serde_json::Value::Null),
            });
        }
    }
    Ok(RowFilter {
        predicates,
        combinator,
    })
}

/// Build a [`RowSort`] from the `sort_by` / `sort_dir` params.
pub(crate) fn parse_sort(
    sort_by: &Option<String>,
    sort_dir: &Option<String>,
) -> Result<RowSort, McpError> {
    let dir = match sort_dir.as_deref() {
        None => SortDir::Desc,
        Some(s) => SortDir::parse(s).ok_or_else(|| invalid("`sort_dir` must be asc|desc"))?,
    };
    let field = match sort_by {
        Some(f) if !f.trim().is_empty() => {
            validate_field_name(f)?;
            Some(f.clone())
        }
        _ => None,
    };
    Ok(RowSort { field, dir })
}

/// Embed a table's `name` (+ `description`) for semantic discovery. Best-effort:
/// a transient embed failure leaves the embedding NULL for the migration cron to
/// backfill (mirrors `experiment` embed-on-write).
pub(crate) async fn embed_table(
    ctx: &SystemContext,
    pool: &PgPool,
    table_id: i64,
    name: &str,
    description: Option<&str>,
) {
    let text = match description {
        Some(d) if !d.trim().is_empty() => format!("{name}\n{d}"),
        _ => name.to_string(),
    };
    match ctx.embed().embed_query(&text).await {
        Ok(v) => {
            if let Err(e) = queries::set_table_embedding(pool, table_id, &v, EMBED_SIGNATURE).await
            {
                tracing::warn!(error = %e, table = %name, "failed to store data-table embedding");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, table = %name,
                "data-table embed-on-write failed; leaving NULL for cron backfill");
        }
    }
}
