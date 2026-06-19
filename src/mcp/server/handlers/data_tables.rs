//! JSON data-table tool handlers (CRUD + analysis + discovery).
//!
//! Thin `#[tool]` forwards into `crate::mcp::tools::data_tables::tool_*`,
//! mirroring the other handler blocks; the per-block router
//! (`router_data_tables`) is composed in `server.rs` via
//! `assembled_tool_router()`.
#![allow(clippy::too_many_lines)]

use crate::mcp::server::McpServer;
use crate::mcp::server::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};

#[rmcp::tool_router(router = router_data_tables, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Create a JSON data table for recording observations (benchmark results, review \
findings, tracked metrics, …). Declaring `columns` makes it strict (rows are type-validated); omit them for a \
free-form JSON table. USE WHEN an agent needs an ad-hoc structured store it can later query / aggregate / \
report on. DO NOT USE WHEN a purpose-built tool fits (work items, experiments, memory). Returns {table, columns}."
    )]
    async fn data_table_create(
        &self,
        Parameters(params): Parameters<DataTableCreateParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "data_table_create",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::data_tables::tool_data_table_create(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Alter a data table: add/drop/modify columns, rename the table, or change its \
description. Renaming a column also migrates the JSON key in existing rows. USE WHEN evolving a table's schema. \
Returns the updated {table, columns}."
    )]
    async fn data_table_alter(
        &self,
        Parameters(params): Parameters<DataTableAlterParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "data_table_alter",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::data_tables::tool_data_table_alter(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Drop a data table and ALL its rows + columns (irreversible). Requires confirm=true \
once the table holds more than a few rows. Returns {dropped, rows_deleted}."
    )]
    async fn data_table_drop(
        &self,
        Parameters(params): Parameters<DataTableDropParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "data_table_drop",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::data_tables::tool_data_table_drop(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "List data tables (with row/column counts), optionally scoped to a project. USE WHEN \
browsing which tables exist. For a fuzzy/semantic lookup use data_table_search. Returns {count, tables}."
    )]
    async fn data_table_list(
        &self,
        Parameters(params): Parameters<DataTableListParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "data_table_list",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::data_tables::tool_data_table_list(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Describe one data table: its schema (columns), row count, and a few sample rows. \
Returns {table, columns, row_count, sample_rows}."
    )]
    async fn data_table_describe(
        &self,
        Parameters(params): Parameters<DataTableDescribeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "data_table_describe",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::data_tables::tool_data_table_describe(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Insert one or more observation rows (each a JSON object) into a data table. Strict \
tables validate every row against the declared column types and apply declared defaults. USE WHEN recording \
observations. Returns {inserted, ids}."
    )]
    async fn data_table_insert(
        &self,
        Parameters(params): Parameters<DataTableInsertParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "data_table_insert",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::data_tables::tool_data_table_insert(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Query rows from a data table with optional field filters (eq/ne/gt/lt/gte/lte/\
contains/exists), sort, and pagination. USE WHEN reading back observations. Returns {returned, total, rows}."
    )]
    async fn data_table_select(
        &self,
        Parameters(params): Parameters<DataTableSelectParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "data_table_select",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::data_tables::tool_data_table_select(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Update rows in a data table by shallow-merging a JSON patch, targeting a single \
row_id or all rows matching a filter (a table-wide update requires all=true). Returns {updated}."
    )]
    async fn data_table_update(
        &self,
        Parameters(params): Parameters<DataTableUpdateParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "data_table_update",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::data_tables::tool_data_table_update(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Delete rows from a data table by row_id or by filter (a table-wide delete requires \
all=true). Returns {deleted}."
    )]
    async fn data_table_delete(
        &self,
        Parameters(params): Parameters<DataTableDeleteParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "data_table_delete",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::data_tables::tool_data_table_delete(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Link a data table to the experiment or work-item it backs (target_type ∈ \
experiment|work_item; optional `role`). Idempotent — re-linking updates the role. Returns \
{link_id, table_id, target_type, target_id}."
    )]
    async fn data_table_link(
        &self,
        Parameters(params): Parameters<DataTableLinkParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "data_table_link",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::data_tables::tool_data_table_link(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Remove a data-table ⇄ experiment/work-item link. Returns {removed}.")]
    async fn data_table_unlink(
        &self,
        Parameters(params): Parameters<DataTableUnlinkParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "data_table_unlink",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::data_tables::tool_data_table_unlink(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Aggregate a data table's rows: group by zero or more fields and compute metrics \
(count | sum | avg | min | max | stddev | median | count_distinct) per group. Non-numeric values are skipped \
and counted (n_ignored). USE WHEN summarizing observations. Returns {table, group_by, total_rows, groups}."
    )]
    async fn data_table_aggregate(
        &self,
        Parameters(params): Parameters<DataTableAggregateParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "data_table_aggregate",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::data_tables::tool_data_table_aggregate(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Render a data table (with optional filter/sort and an aggregation summary) into a \
formatted report: markdown | org | latex | html | text (unicode box-drawing) | json | csv. Optionally writes \
the report to a file (OVERWRITES). USE WHEN producing a human-readable or exportable report. Returns the \
rendered text, or {path, bytes_written} when written."
    )]
    async fn data_table_report(
        &self,
        Parameters(params): Parameters<DataTableReportParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "data_table_report",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::data_tables::tool_data_table_report(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Find data tables by semantic similarity of their name+description to a query (BGE-M3 \
embeddings). USE WHEN you don't know a table's exact name (e.g. \"which tables track latency?\"). Tables are \
embedded asynchronously, so a brand-new table may not appear until its embedding lands. Returns {query, count, results}."
    )]
    async fn data_table_search(
        &self,
        Parameters(params): Parameters<DataTableSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "data_table_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::data_tables::tool_data_table_search(self.ctx(), params),
        )
        .await
    }
}
