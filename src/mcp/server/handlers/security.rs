//! SOTA security-analysis handlers.
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

#[rmcp::tool_router(router = router_security, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Surface co-located taint sources (env/request/stdin) and sinks (exec/eval/SQL) per file. \
Newsome-Song NDSS 2005 audit aid."
    )]
    async fn taint_analysis(
        &self,
        Parameters(params): Parameters<TaintAnalysisParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "taint_analysis",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_taint_analysis::tool_taint_analysis(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Hardcoded-secret detection via entropy + known-prefix regex (Meli et al. NDSS 2019)."
    )]
    async fn secret_detection(
        &self,
        Parameters(params): Parameters<SecretDetectionParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "secret_detection",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_secret_detection::tool_secret_detection(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Crypto-misuse rules (CryptoLint CCS 2013): ECB mode, MD5/SHA-1 in auth, weak RNG for tokens, static IVs, hardcoded keys."
    )]
    async fn crypto_misuse(
        &self,
        Parameters(params): Parameters<CryptoMisuseParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "crypto_misuse",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_crypto_misuse::tool_crypto_misuse(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "CWE-502 unsafe-deserialization patterns: pickle.loads, yaml.load, ObjectInputStream, etc."
    )]
    async fn unsafe_deserialization(
        &self,
        Parameters(params): Parameters<UnsafeDeserializationParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "unsafe_deserialization",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_unsafe_deserialization::tool_unsafe_deserialization(
                self.ctx(),
                params,
            ),
        )
        .await
    }
    #[tool(
        description = "SQL / shell injection candidates: string concatenation into exec/query calls."
    )]
    async fn injection_candidates(
        &self,
        Parameters(params): Parameters<InjectionCandidatesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "injection_candidates",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_injection_candidates::tool_injection_candidates(
                self.ctx(),
                params,
            ),
        )
        .await
    }
    #[tool(
        description = "Mutating HTTP routes (POST/PUT/DELETE/PATCH) in files lacking visible auth middleware."
    )]
    async fn unprotected_routes(
        &self,
        Parameters(params): Parameters<UnprotectedRoutesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "unprotected_routes",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_unprotected_routes::tool_unprotected_routes(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Parse Cargo.lock / package-lock.json / requirements.txt and surface dependencies for OSV.dev review."
    )]
    async fn cve_supply_chain(
        &self,
        Parameters(params): Parameters<CveSupplyChainParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "cve_supply_chain",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_cve_supply_chain::tool_cve_supply_chain(self.ctx(), params),
        )
        .await
    }
}
