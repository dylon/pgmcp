//! SOTA ML / embedding-based handlers.
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

#[rmcp::tool_router(router = router_ml_embedding, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "LSH clone detection on 384-d embeddings (Indyk-Motwani STOC 1998). SimHash + banded LSH gives O(1) candidate retrieval; rerank by exact cosine."
    )]
    async fn lsh_clone_detection(
        &self,
        Parameters(params): Parameters<LshCloneDetectionParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "lsh_clone_detection",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_lsh_clone_detection::tool_lsh_clone_detection(
                self.ctx(),
                params,
            ),
        )
        .await
    }
    #[tool(
        description = "Semantic drift per file: cosine distance between current centroid and 30+-day historical centroid (Hamilton et al. ACL 2016)."
    )]
    async fn semantic_drift(
        &self,
        Parameters(params): Parameters<SemanticDriftParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "semantic_drift",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_semantic_drift::tool_semantic_drift(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "LOF-style embedding outliers (Breunig et al. SIGMOD 2000): chunks whose mean k-NN cosine distance is unusually high."
    )]
    async fn embedding_outliers(
        &self,
        Parameters(params): Parameters<EmbeddingOutliersParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "embedding_outliers",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_embedding_outliers::tool_embedding_outliers(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Multi-resolution PageRank: PageRank within each Louvain community + PageRank on the community-supernode graph. Surfaces both module leaders and module importance."
    )]
    async fn multi_resolution_pagerank(
        &self,
        Parameters(params): Parameters<MultiResolutionPagerankParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "multi_resolution_pagerank",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_multi_resolution_pagerank::tool_multi_resolution_pagerank(
                self.ctx(),
                params,
            ),
        )
        .await
    }
}
