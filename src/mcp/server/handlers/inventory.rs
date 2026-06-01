//! Inventory & introspection handlers (project tree, file info, stats, telemetry, reindex).
//!
//! Tool handlers extracted verbatim from `server.rs` (B.3 god-file split).
//! Only the relative `super::tools::` path was rewritten to the absolute
//! `crate::mcp::tools::`; bodies are otherwise identical. The per-block
//! router is composed in `server.rs` via `assembled_tool_router()`.
#![allow(clippy::too_many_lines)]

use crate::mcp::server::McpServer;
use crate::mcp::server::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};

#[rmcp::tool_router(router = router_inventory, vis = "pub(crate)")]
impl McpServer {
    #[tool(description = "Project file tree limited by depth (depth=2 typical). \
USE WHEN: you want the structural overview of a project without enumerating every file \
yourself via `Glob`. \
DO NOT USE WHEN: you only need to glob within a specific subdirectory — the built-in \
`Glob` tool gives you exact pattern matching against the live filesystem. \
For unfamiliar projects, prefer `orient` which bundles project_tree, top topics, and key \
entry points.")]
    async fn project_tree(
        &self,
        Parameters(params): Parameters<ProjectTreeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "project_tree",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_project_tree::tool_project_tree(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Indexed-file metadata envelope (size, language, line count, \
last_indexed_at, project name, chunk count). \
USE WHEN: you want a quick fingerprint of a file before deciding whether to read it, \
or before semantic_search/grep on it specifically. \
DO NOT USE WHEN: the file is not in the index (e.g., just written, .gitignore'd) — \
use the built-in `Bash: stat` or `Read` instead."
    )]
    async fn file_info(
        &self,
        Parameters(params): Parameters<FileInfoParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "file_info",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_file_info::tool_file_info(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Get overall indexing statistics including file counts, search counts, and pool state."
    )]
    async fn index_stats(
        &self,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "index_stats",
            30,
            &_ctx,
            "",
            crate::mcp::tools::tool_index_stats::tool_index_stats(self.ctx()),
        )
        .await
    }

    #[tool(
        description = "Query per-call MCP tool telemetry from the durable `mcp_tool_calls` table. \
USE WHEN: you want a historical view of which tools were used (over the last N minutes), how long they took (p50/p95/p99), which agents called them, and which projects they targeted. \
DO NOT USE WHEN: you only need real-time counts — `index_stats` and the `pgmcp://stats` resource already carry the live in-memory snapshot. \
Aggregation modes: `summary` (default; (tool × client × project) breakdown with percentiles), `top_tools`, `top_callers`, `top_projects`, `error_rate`, `histogram` (log-spaced duration bands), `raw` (most-recent rows). \
Default lookback is 60 minutes; pass `since_minutes` up to 44640 (31 days) to widen it."
    )]
    async fn mcp_tool_telemetry(
        &self,
        Parameters(params): Parameters<McpToolTelemetryParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "mcp_tool_telemetry",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_mcp_tool_telemetry::tool_mcp_tool_telemetry(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Trigger a re-index. With no `language`, clears the entire index and \
restarts indexing (long-running task). With `language` (e.g. \"latex\"), re-extracts only that \
language's files — the narrow way to re-apply an extractor change while preserving every other \
file's incremental skip."
    )]
    async fn reindex(
        &self,
        Parameters(params): Parameters<ReindexParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // No timeout: reindex can run for minutes on a large workspace.
        // Progress is reported via the MCP task store, not the immediate
        // response — wrapping in 30s would falsely fail every full reindex.
        // Routed through `instrumented_tool_run` (not `instrumented_tool_wrap`)
        // so the central tracing events still fire while skipping `timeout_wrap`.
        let caller = extract_caller(&_ctx);
        let request_id = Some(format!("{:?}", _ctx.id));
        let mcp_session_id = extract_mcp_session_id(&_ctx);
        instrumented_tool_run(
            self.stats(),
            "reindex",
            None,
            caller,
            "",
            request_id,
            mcp_session_id,
            crate::mcp::tools::tool_reindex::tool_reindex(self.ctx(), params),
        )
        .await
    }
}
