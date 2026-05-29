//! SOTA information-theory & evolution handlers.
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

#[rmcp::tool_router(router = router_infotheory, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Normalized Compression Distance (Cilibrasi-Vitányi 2005). \
NCD(x,y) ≈ 0 = clones; ≈ 1 = unrelated. Uses zstd as compressor. \
USE WHEN: clone detection across languages — catches parametric clones embedding-cosine misses."
    )]
    async fn compression_distance(
        &self,
        Parameters(params): Parameters<CompressionDistanceParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "compression_distance",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_compression_distance::tool_compression_distance(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(description = "Mutual information for git co-change. \
Sharper than Jaccard — penalizes coincidental overlap with high-frequency files. \
USE WHEN: identifying causally-coupled refactor candidates from history.")]
    async fn cochange_mutual_information(
        &self,
        Parameters(params): Parameters<CochangeMutualInformationParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "cochange_mutual_information",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_cochange_mutual_information::tool_cochange_mutual_information(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(description = "Conditional entropy of imports H(target | source). \
USE WHEN: spotting broker modules (high entropy → imports spread across many targets) vs focused dependencies.")]
    async fn import_entropy(
        &self,
        Parameters(params): Parameters<ImportEntropyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "import_entropy",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_import_entropy::tool_import_entropy(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Shannon entropy of identifier-token distribution per file (Abebe et al. ICPC 2009). \
USE WHEN: spotting naming pollution / generated code (low entropy) vs clear domain vocabulary (high)."
    )]
    async fn identifier_entropy(
        &self,
        Parameters(params): Parameters<IdentifierEntropyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "identifier_entropy",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_identifier_entropy::tool_identifier_entropy(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // SOTA Phase 4 — Evolution + quality
    // ========================================================================

    #[tool(description = "Bus factor per file (Avelino et al. ICSE 2016). \
USE WHEN: finding single points of failure in maintainership — files where one or two authors own >= 50% of lines.")]
    async fn bus_factor(
        &self,
        Parameters(params): Parameters<BusFactorParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "bus_factor",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_bus_factor::tool_bus_factor(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Knowledge-silo detection via Gini + Herfindahl on blame distribution. \
USE WHEN: identifying single-author files / concentration-of-knowledge risks."
    )]
    async fn knowledge_silos(
        &self,
        Parameters(params): Parameters<KnowledgeSilosParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "knowledge_silos",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_knowledge_silos::tool_knowledge_silos(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Ownership-coupling mismatch: files that co-change frequently but have disjoint author sets. \
USE WHEN: predicting merge-conflict-prone refactor candidates."
    )]
    async fn ownership_coupling_mismatch(
        &self,
        Parameters(params): Parameters<OwnershipCouplingMismatchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "ownership_coupling_mismatch",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_ownership_coupling_mismatch::tool_ownership_coupling_mismatch(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Doc-code drift: cosine distance between doc-chunk and code-chunk embedding centroids per directory. \
USE WHEN: finding stale documentation whose vocabulary has diverged from the code."
    )]
    async fn doc_code_drift(
        &self,
        Parameters(params): Parameters<DocCodeDriftParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "doc_code_drift",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_doc_code_drift::tool_doc_code_drift(self.ctx(), params),
        )
        .await
    }
}
