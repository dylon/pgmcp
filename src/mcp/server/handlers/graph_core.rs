//! Core dependency-graph & impact handlers.
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

#[rmcp::tool_router(router = router_graph_core, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Project dependency graph: import relationships, optionally focused on a \
file's neighborhood. \
USE WHEN: you need to know what depends on a file, what a file depends on, or want a \
Graphviz diagram of an architecture. \
DO NOT USE WHEN: you need co-change behavior (use `find_coupled_files`) or static call \
graphs (this is import-level only). \
Output formats: summary (counts), edges (list), DOT (Graphviz). Requires graph-analysis cron."
    )]
    async fn dependency_graph(
        &self,
        Parameters(params): Parameters<DependencyGraphParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "dependency_graph",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_dependency_graph::tool_dependency_graph(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Rank files by graph centrality (PageRank, betweenness, degree). \
USE WHEN: identifying load-bearing files in an unfamiliar codebase ('what should I read \
first?'), or finding which files a refactor would impact most. High-centrality = touches \
many other files. \
DO NOT USE WHEN: you want change-frequency or bug-proneness — use `bug_prediction` or \
`complexity_hotspots`. \
Requires graph-analysis cron. The composite `orient` tool returns the top entry points by \
PageRank as part of its envelope."
    )]
    async fn centrality_analysis(
        &self,
        Parameters(params): Parameters<CentralityAnalysisParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "centrality_analysis",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_centrality_analysis::tool_centrality_analysis(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Detect module communities in the dependency graph using Louvain algorithm. Compares discovered communities against directory structure to reveal architectural misalignment. Requires the graph-analysis cron job to have run."
    )]
    async fn community_detection(
        &self,
        Parameters(params): Parameters<CommunityDetectionParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "community_detection",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_community_detection::tool_community_detection(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Find circular import dependency cycles (Tarjan SCC + DFS). \
USE WHEN: investigating build/link errors, code that's hard to test in isolation, or \
auditing layering violations. Cycles make code harder to test, build, and understand. \
DO NOT USE WHEN: looking for runtime call cycles (this is import-level static graph only). \
Requires graph-analysis cron."
    )]
    async fn circular_dependencies(
        &self,
        Parameters(params): Parameters<CircularDependenciesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "circular_dependencies",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_circular_dependencies::tool_circular_dependencies(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Predict which files would be affected by changing a specific file. \
USE WHEN: scoping a refactor or assessing the blast radius of a change before making it. \
Combines reverse-imports + git co-change + semantic similarity for richer impact than any \
single signal. \
DO NOT USE WHEN: you only need static reverse-imports (use `dependency_graph` with focus). \
Requires graph-analysis cron + git history for full coverage."
    )]
    async fn change_impact_analysis(
        &self,
        Parameters(params): Parameters<ChangeImpactAnalysisParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "change_impact_analysis",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_change_impact_analysis::tool_change_impact_analysis(
                self.ctx(),
                params,
            ),
        )
        .await
    }
}
