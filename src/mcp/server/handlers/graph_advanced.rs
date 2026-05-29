//! SOTA graph-algorithm handlers (k-core, PPR, betweenness, motifs).
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

#[rmcp::tool_router(router = router_graph_advanced, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "K-core decomposition (Seidman 1983, Batagelj-Zaversnik O(m) peeling). \
USE WHEN: identifying load-bearing structural backbone vs the periphery. \
Returns each file's coreness (highest k such that the file is in a k-core)."
    )]
    async fn kcore_analysis(
        &self,
        Parameters(params): Parameters<KcoreAnalysisParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "kcore_analysis",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_kcore_analysis::tool_kcore_analysis(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "K-truss decomposition (Cohen 2008): per-edge trussness via triangle support peeling. \
USE WHEN: finding cohesive dense regions and fragile single-triangle edges."
    )]
    async fn ktruss_analysis(
        &self,
        Parameters(params): Parameters<KtrussAnalysisParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "ktruss_analysis",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_ktruss_analysis::tool_ktruss_analysis(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Personalized PageRank with restart (Tong-Faloutsos-Pan ICDM 2006). \
USE WHEN: computing blast radius from a seed set — how much does each file depend on the seeds? \
Sharper than vanilla PageRank for targeted impact analysis."
    )]
    async fn personalized_pagerank(
        &self,
        Parameters(params): Parameters<PersonalizedPagerankParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "personalized_pagerank",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_personalized_pagerank::tool_personalized_pagerank(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Edge betweenness centrality (Brandes 2001 edge variant of Girvan-Newman 2002). \
USE WHEN: finding bottleneck import edges that route many shortest paths — removing them would split or stretch the dependency graph."
    )]
    async fn edge_betweenness(
        &self,
        Parameters(params): Parameters<EdgeBetweennessParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "edge_betweenness",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_edge_betweenness::tool_edge_betweenness(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Burt's structural-holes constraint (Burt 1992). \
USE WHEN: identifying broker files that bridge otherwise-disconnected neighbourhoods (low constraint = high-leverage broker). \
DO NOT USE: as a betweenness substitute — constraint measures redundancy, not paths.")]
    async fn structural_holes(
        &self,
        Parameters(params): Parameters<StructuralHolesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "structural_holes",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_structural_holes::tool_structural_holes(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Motif / graphlet census (Milo et al. Science 2002, Pržulj GDD 2007). \
USE WHEN: characterizing architecture-signature — high 030T = clean layering, high 030C = circular deps, high cliques = god-cluster."
    )]
    async fn motif_census(
        &self,
        Parameters(params): Parameters<MotifCensusParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "motif_census",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_motif_census::tool_motif_census(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Modularity-based attack vulnerability (Holme et al. PRE 2002). \
Simulates sequential file removal by chosen order (pagerank / betweenness / degree) and tracks the largest connected component. \
USE WHEN: quantifying architectural resilience against single-file outages."
    )]
    async fn attack_vulnerability(
        &self,
        Parameters(params): Parameters<AttackVulnerabilityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "attack_vulnerability",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_attack_vulnerability::tool_attack_vulnerability(
                self.ctx(),
                params,
            ),
        )
        .await
    }
}
