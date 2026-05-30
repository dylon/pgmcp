//! Parameter types for the `data_table_*` tool family (JSON data tables).
//!
//! Re-exported by `params/mod.rs` (and transitively by `server.rs`) so every
//! `crate::mcp::server::DataTable*Params` resolves for the tool bodies. Follows
//! the established idiom: `#[derive(Debug, Deserialize, schemars::JsonSchema)]`
//! with per-field `#[schemars(description = …)]`. All tools accept a table by
//! `table` (name, optionally project-scoped) or `table_id` (numeric).
#![allow(unused_imports)]

use super::*;
use rmcp::schemars;
use serde::Deserialize;

// ── Shared nested input structs ──────────────────────────────────────────────

/// A column declaration for `data_table_create` / `data_table_alter`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ColumnDefParam {
    #[schemars(description = "Column name (lowercase, [a-z][a-z0-9_]*, ≤63 chars).")]
    pub name: String,
    #[schemars(description = "Type: text | integer | number | boolean | timestamp | json.")]
    pub data_type: String,
    #[schemars(description = "Whether the field is required on insert (default false).")]
    pub required: Option<bool>,
    #[schemars(description = "Default JSON value applied to absent fields on insert.")]
    pub default: Option<serde_json::Value>,
    #[schemars(description = "Optional human description of the column.")]
    pub description: Option<String>,
}

/// A column modification for `data_table_alter.modify_columns`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ColumnPatchParam {
    #[schemars(description = "Existing column name to modify.")]
    pub name: String,
    #[schemars(description = "New required flag (omit to leave unchanged).")]
    pub required: Option<bool>,
    #[schemars(description = "New default JSON value (omit to leave unchanged).")]
    pub default: Option<serde_json::Value>,
    #[schemars(
        description = "Rename the column to this name (also renames the JSON key in rows)."
    )]
    pub rename_to: Option<String>,
}

/// One filter predicate over a JSON field.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct FilterClauseParam {
    #[schemars(description = "JSON field name to test.")]
    pub field: String,
    #[schemars(description = "Operator: eq | ne | gt | lt | gte | lte | contains | exists.")]
    pub op: String,
    #[schemars(description = "Comparison value (omit only for op=exists).")]
    pub value: Option<serde_json::Value>,
}

/// One aggregation metric for `data_table_aggregate` / a report summary.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct AggSpecParam {
    #[schemars(description = "Field to aggregate (omit only for func=count of rows).")]
    pub field: Option<String>,
    #[schemars(
        description = "Function: count | sum | avg | min | max | stddev | median | count_distinct."
    )]
    pub func: String,
    #[schemars(description = "Output key for this metric (default \"{func}_{field}\").")]
    pub alias: Option<String>,
}

/// The embedded aggregation summary block for `data_table_report`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ReportSummaryParam {
    #[schemars(description = "Fields to group the summary by (omit for one overall group).")]
    pub group_by: Option<Vec<String>>,
    #[schemars(description = "Aggregations to compute for the summary section.")]
    pub aggregations: Vec<AggSpecParam>,
}

// ── Table DDL ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DataTableCreateParams {
    #[schemars(
        description = "Table name (lowercase, [a-z][a-z0-9_]*, ≤63 chars). Unique per scope."
    )]
    pub name: String,
    #[schemars(description = "Optional human description (embedded for semantic discovery).")]
    pub description: Option<String>,
    #[schemars(description = "Optional project name to scope the table to (else global).")]
    pub project: Option<String>,
    #[schemars(
        description = "Optional typed columns. Declaring any makes the table strict (rows validated); omit for a free-form JSON table."
    )]
    pub columns: Option<Vec<ColumnDefParam>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DataTableAlterParams {
    #[schemars(description = "Table name (or use table_id).")]
    pub table: Option<String>,
    #[schemars(description = "Numeric table id (or use table).")]
    pub table_id: Option<i64>,
    #[schemars(description = "Project scope for name resolution.")]
    pub project: Option<String>,
    #[schemars(description = "Columns to add.")]
    pub add_columns: Option<Vec<ColumnDefParam>>,
    #[schemars(description = "Column names to drop.")]
    pub drop_columns: Option<Vec<String>>,
    #[schemars(description = "Columns to modify (required / default / rename).")]
    pub modify_columns: Option<Vec<ColumnPatchParam>>,
    #[schemars(description = "Rename the table to this name.")]
    pub rename_to: Option<String>,
    #[schemars(description = "Replace the table description.")]
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DataTableDropParams {
    #[schemars(description = "Table name (or use table_id).")]
    pub table: Option<String>,
    #[schemars(description = "Numeric table id (or use table).")]
    pub table_id: Option<i64>,
    #[schemars(description = "Project scope for name resolution.")]
    pub project: Option<String>,
    #[schemars(
        description = "Required (must be true) when the table holds more than a few rows — guards against accidental drops."
    )]
    pub confirm: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DataTableListParams {
    #[schemars(description = "Optional project name to scope the listing (else all tables).")]
    pub project: Option<String>,
    #[schemars(description = "Max tables to return (default 100).")]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DataTableDescribeParams {
    #[schemars(description = "Table name (or use table_id).")]
    pub table: Option<String>,
    #[schemars(description = "Numeric table id (or use table).")]
    pub table_id: Option<i64>,
    #[schemars(description = "Project scope for name resolution.")]
    pub project: Option<String>,
    #[schemars(description = "How many sample rows to include (default 5, max 50).")]
    pub sample: Option<i64>,
}

// ── Row DML ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DataTableInsertParams {
    #[schemars(description = "Table name (or use table_id).")]
    pub table: Option<String>,
    #[schemars(description = "Numeric table id (or use table).")]
    pub table_id: Option<i64>,
    #[schemars(description = "Project scope for name resolution.")]
    pub project: Option<String>,
    #[schemars(description = "Rows to insert; each a JSON object. Strict tables validate each.")]
    pub rows: Vec<serde_json::Value>,
    #[schemars(description = "Optional free-text provenance recorded on every inserted row.")]
    pub source: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DataTableSelectParams {
    #[schemars(description = "Table name (or use table_id).")]
    pub table: Option<String>,
    #[schemars(description = "Numeric table id (or use table).")]
    pub table_id: Option<i64>,
    #[schemars(description = "Project scope for name resolution.")]
    pub project: Option<String>,
    #[schemars(description = "Filter predicates over JSON fields.")]
    pub filter: Option<Vec<FilterClauseParam>>,
    #[schemars(description = "How predicates combine: all (AND, default) | any (OR).")]
    pub combine: Option<String>,
    #[schemars(description = "JSON field to sort by (default: newest first).")]
    pub sort_by: Option<String>,
    #[schemars(description = "Sort direction: asc | desc (default desc).")]
    pub sort_dir: Option<String>,
    #[schemars(description = "Max rows (default 100, max 1000).")]
    pub limit: Option<i64>,
    #[schemars(description = "Offset for pagination (default 0).")]
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DataTableUpdateParams {
    #[schemars(description = "Table name (or use table_id).")]
    pub table: Option<String>,
    #[schemars(description = "Numeric table id (or use table).")]
    pub table_id: Option<i64>,
    #[schemars(description = "Project scope for name resolution.")]
    pub project: Option<String>,
    #[schemars(description = "Update a single row by its id (or use filter).")]
    pub row_id: Option<i64>,
    #[schemars(description = "Update all rows matching these predicates (or use row_id).")]
    pub filter: Option<Vec<FilterClauseParam>>,
    #[schemars(description = "How predicates combine: all (AND, default) | any (OR).")]
    pub combine: Option<String>,
    #[schemars(description = "JSON object shallow-merged into each matched row's data.")]
    pub patch: serde_json::Value,
    #[schemars(
        description = "Set true to allow updating every row when neither row_id nor filter is given (guards against accidental table-wide updates)."
    )]
    pub all: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DataTableDeleteParams {
    #[schemars(description = "Table name (or use table_id).")]
    pub table: Option<String>,
    #[schemars(description = "Numeric table id (or use table).")]
    pub table_id: Option<i64>,
    #[schemars(description = "Project scope for name resolution.")]
    pub project: Option<String>,
    #[schemars(description = "Delete a single row by its id (or use filter).")]
    pub row_id: Option<i64>,
    #[schemars(description = "Delete all rows matching these predicates (or use row_id).")]
    pub filter: Option<Vec<FilterClauseParam>>,
    #[schemars(description = "How predicates combine: all (AND, default) | any (OR).")]
    pub combine: Option<String>,
    #[schemars(
        description = "Set true to allow deleting every row when neither row_id nor filter is given (guards against accidental table wipes)."
    )]
    pub all: Option<bool>,
}

// ── Analysis & report ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DataTableAggregateParams {
    #[schemars(description = "Table name (or use table_id).")]
    pub table: Option<String>,
    #[schemars(description = "Numeric table id (or use table).")]
    pub table_id: Option<i64>,
    #[schemars(description = "Project scope for name resolution.")]
    pub project: Option<String>,
    #[schemars(description = "Filter predicates applied before aggregation.")]
    pub filter: Option<Vec<FilterClauseParam>>,
    #[schemars(description = "How predicates combine: all (AND, default) | any (OR).")]
    pub combine: Option<String>,
    #[schemars(description = "Fields to group by (omit for one overall group).")]
    pub group_by: Option<Vec<String>>,
    #[schemars(description = "Aggregations to compute per group.")]
    pub aggregations: Vec<AggSpecParam>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DataTableReportParams {
    #[schemars(description = "Table name (or use table_id).")]
    pub table: Option<String>,
    #[schemars(description = "Numeric table id (or use table).")]
    pub table_id: Option<i64>,
    #[schemars(description = "Project scope for name resolution.")]
    pub project: Option<String>,
    #[schemars(description = "Filter predicates over JSON fields.")]
    pub filter: Option<Vec<FilterClauseParam>>,
    #[schemars(description = "How predicates combine: all (AND, default) | any (OR).")]
    pub combine: Option<String>,
    #[schemars(description = "JSON field to sort by (default: newest first).")]
    pub sort_by: Option<String>,
    #[schemars(description = "Sort direction: asc | desc (default desc).")]
    pub sort_dir: Option<String>,
    #[schemars(description = "Max detail rows to render (default 100).")]
    pub limit: Option<i64>,
    #[schemars(description = "Project only these columns (default: all, schema order).")]
    pub columns: Option<Vec<String>>,
    #[schemars(
        description = "Output format: markdown (default) | org | latex | html | text | json | csv."
    )]
    pub format: Option<String>,
    #[schemars(description = "Report title (default: the table name).")]
    pub title: Option<String>,
    #[schemars(description = "Optional caption rendered under the table.")]
    pub caption: Option<String>,
    #[schemars(description = "Optional aggregation summary rendered above the detail table.")]
    pub summary: Option<ReportSummaryParam>,
    #[schemars(
        description = "If set, WRITE the rendered report to this path (cwd-relative; OVERWRITES) and return {path, bytes_written}; else return the rendered text."
    )]
    pub write_to_path: Option<String>,
}

// ── Discovery ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DataTableSearchParams {
    #[schemars(
        description = "Natural-language query; ranks tables by name+description similarity."
    )]
    pub query: String,
    #[schemars(description = "Optional project name to scope the search.")]
    pub project: Option<String>,
    #[schemars(description = "Max tables to return (default 10).")]
    pub limit: Option<i64>,
}
