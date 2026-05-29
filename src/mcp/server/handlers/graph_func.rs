//! Function-level call-graph analytics & connectivity handlers.
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

#[rmcp::tool_router(router = router_graph_func, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Function-level centrality (PageRank / betweenness / harmonic / coreness) over the \
symbol-resolved call graph, read from `function_metrics`. \
USE WHEN: finding the load-bearing functions to read or refactor first — sharper than file-level `centrality_analysis`."
    )]
    async fn central_functions(
        &self,
        Parameters(params): Parameters<CentralFunctionsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "central_functions",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_central_functions::tool_central_functions(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Louvain communities over the call graph — cross-file functional clusters. \
USE WHEN: recovering the real modular structure and spotting concerns smeared across many files vs. the directory layout."
    )]
    async fn function_communities(
        &self,
        Parameters(params): Parameters<FunctionCommunitiesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "function_communities",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_function_communities::tool_function_communities(
                self.ctx(),
                params,
            ),
        )
        .await
    }
    #[tool(
        description = "k-core decomposition over the call graph — functions in the densely interconnected \
execution core. USE WHEN: locating the tangled architectural nucleus that resists being split out."
    )]
    async fn function_kcore(
        &self,
        Parameters(params): Parameters<FunctionKcoreParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "function_kcore",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_function_kcore::tool_function_kcore(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Mutual- and direct-recursion in the call graph (strongly-connected components of size ≥ 2 \
plus the concrete call cycles inside them). USE WHEN: finding unintended call cycles that block layering, \
complicate testing, and risk unbounded recursion."
    )]
    async fn recursive_clusters(
        &self,
        Parameters(params): Parameters<RecursiveClustersParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "recursive_clusters",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_recursive_clusters::tool_recursive_clusters(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Extended centrality (eigenvector / Katz / harmonic / closeness / reverse-PageRank) over \
the file import graph or the function call graph. USE WHEN: `centrality_analysis` (PageRank / betweenness / degree) \
isn't the right lens — influence among important neighbours (eigenvector/Katz), reach (harmonic/closeness), or \
foundational sinks everything depends on (reverse-PageRank)."
    )]
    async fn extended_centrality(
        &self,
        Parameters(params): Parameters<ExtendedCentralityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "extended_centrality",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_extended_centrality::tool_extended_centrality(
                self.ctx(),
                params,
            ),
        )
        .await
    }
    #[tool(
        description = "Articulation points (cut vertices) + bridges (cut edges) over the file import graph \
or the function call graph (Hopcroft-Tarjan). USE WHEN: finding true structural single points of failure — \
nodes/edges whose removal disconnects the graph — sharper than the ownership-based `bus_factor`."
    )]
    async fn articulation_points(
        &self,
        Parameters(params): Parameters<ArticulationPointsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "articulation_points",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_articulation_points::tool_articulation_points(
                self.ctx(),
                params,
            ),
        )
        .await
    }
    #[tool(
        description = "Connectivity & decoupling structure over the file import graph or function call \
graph: 2-edge-connected components (Tarjan — subsystems robust to single-edge failure), global min-cut \
(Stoer-Wagner — the weakest seam to split a subsystem), and Leiden well-connectedness refinement of \
Louvain (how many communities were internally disconnected). USE WHEN: assessing structural robustness or \
finding a concrete module-decoupling boundary."
    )]
    async fn graph_connectivity(
        &self,
        Parameters(params): Parameters<GraphConnectivityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "graph_connectivity",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_graph_connectivity::tool_graph_connectivity(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Chidamber-Kemerer OO metrics per class: WMC (Σ method cyclomatic), DIT (inheritance \
depth), NOC (number of children), CBO (coupling), RFC (response for class). USE WHEN: finding complex / \
deeply-inherited / heavily-coupled classes to refactor or prioritize for testing. DIT/NOC need the \
language's inherit/impl edges (Python & C/C++ emit them today)."
    )]
    async fn ck_metrics(
        &self,
        Parameters(params): Parameters<CkMetricsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "ck_metrics",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_ck_metrics::tool_ck_metrics(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Spectral analysis over the file import graph or function call graph: algebraic \
connectivity λ₂ + Fiedler bisection (Fiedler/Shi-Malik — global robustness + a balanced split boundary) \
and Weisfeiler-Lehman structural clones (repeated call/import shapes despite renamed identifiers). USE \
WHEN: gauging how bottlenecked the architecture is, finding a natural module split, or detecting \
structural (not textual) duplication."
    )]
    async fn spectral_analysis(
        &self,
        Parameters(params): Parameters<SpectralAnalysisParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "spectral_analysis",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_spectral_analysis::tool_spectral_analysis(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Design Structure Matrix metrics: propagation cost + Core/Shared/Control/Peripheral \
classification over the file import graph or function call graph (MacCormack-Rusnak-Baldwin). USE WHEN: \
quantifying overall coupling (what fraction of the system a change ripples through) and locating the \
architectural core, change-risk hubs (high visibility fan-in), and widest-blast-radius files (high fan-out)."
    )]
    async fn architecture_dsm(
        &self,
        Parameters(params): Parameters<ArchitectureDsmParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "architecture_dsm",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_architecture_dsm::tool_architecture_dsm(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Graph-aware code retrieval via Personalized PageRank (HippoRAG) over the code \
graph (import/call/co_change/semantic). USE WHEN: a query is relational ('how does X flow to Y', 'what \
configures Z') — PPR pulls in callers/callees/config one or two hops from the lexical hits that flat \
`semantic_search` / `hybrid_search` miss, in one shot."
    )]
    async fn code_ppr_search(
        &self,
        Parameters(params): Parameters<CodePprSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "code_ppr_search",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_code_ppr_search::tool_code_ppr_search(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "PathRAG over the code graph: ranked, flow-pruned dependency ROUTES (import/call/\
co_change/semantic chains) from the query's dense-similar files. USE WHEN: tracing 'how does A reach B' \
or the strongest chain linking a query hit to related code — returns the actual paths, where \
`code_ppr_search` returns ranked files."
    )]
    async fn code_path_search(
        &self,
        Parameters(params): Parameters<CodePathSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "code_path_search",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_code_path_search::tool_code_path_search(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "RAPTOR-over-code: query precomputed conceptual cluster summaries (module gists). \
USE WHEN: a query is conceptual ('where does this project handle retries', 'which module owns auth') and \
no single chunk answers it; omit `project` to compare modules across ALL indexed projects. Requires the \
`code-raptor` cron to have run."
    )]
    async fn code_raptor_search(
        &self,
        Parameters(params): Parameters<CodeRaptorSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "code_raptor_search",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_code_raptor_search::tool_code_raptor_search(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "HITS hubs & authorities over the file import graph or the function call graph \
(Kleinberg). USE WHEN: separating orchestrators (hubs) from core utilities (authorities) — a split \
PageRank conflates into one score."
    )]
    async fn hits(
        &self,
        Parameters(params): Parameters<HitsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "hits",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_hits::tool_hits(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Dominator-tree chokepoints from an entry node over the file import or function call \
graph (Cooper-Harvey-Kennedy). USE WHEN: finding must-pass-through funnels — nodes every path from the \
root traverses — to place caching/validation/boundaries or assess blast radius."
    )]
    async fn dominator_tree(
        &self,
        Parameters(params): Parameters<DominatorTreeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "dominator_tree",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_dominator_tree::tool_dominator_tree(self.ctx(), params),
        )
        .await
    }
}
