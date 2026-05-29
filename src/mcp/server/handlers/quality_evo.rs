//! SOTA test-quality & concurrency-safety handlers (part A).
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

#[rmcp::tool_router(router = router_quality_evo, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Test-smell detection (van Deursen et al. XP 2001; Garousi JSS 2018). \
Detects Assertion Roulette, Mystery Guest, Conditional Logic in Tests, Eager Test."
    )]
    async fn test_smells(
        &self,
        Parameters(params): Parameters<TestSmellsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "test_smells",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_test_smells::tool_test_smells(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Mutation-testing surrogate (Just et al. FSE 2014): per-file ratio of commits that change source without changing tests."
    )]
    async fn mutation_score_surrogate(
        &self,
        Parameters(params): Parameters<MutationScoreSurrogateParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "mutation_score_surrogate",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_mutation_score_surrogate::tool_mutation_score_surrogate(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Flaky-test candidates (Luo et al. FSE 2014; Lam et al. ASE 2019). \
Heuristic over commit messages mentioning flakiness/race/retry/timing near test edits."
    )]
    async fn flaky_test_candidates(
        &self,
        Parameters(params): Parameters<FlakyTestCandidatesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "flaky_test_candidates",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_flaky_test_candidates::tool_flaky_test_candidates(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    // ========================================================================
    // SOTA Phase 5 — Concurrency / safety / performance
    // ========================================================================
    #[tool(
        description = "Detect lock-acquisition sites across Rust/C++/Java/Go/Python. \
Eraser-style lockset analysis (Savage et al. TOCS 1997) audit aid."
    )]
    async fn lockset_races(
        &self,
        Parameters(params): Parameters<LocksetRacesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "lockset_races",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_lockset_races::tool_lockset_races(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Per-file `unsafe` block density (Astrauskas OOPSLA 2020). \
Concentration of unsafe in non-FFI files = review priority."
    )]
    async fn unsafe_clusters(
        &self,
        Parameters(params): Parameters<UnsafeClustersParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "unsafe_clusters",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_unsafe_clusters::tool_unsafe_clusters(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Per-function panic-leaf count (panic!/unwrap/expect/assert) from `function_metrics`. \
USE WHEN: hunting Rust library footguns that crash on unexpected input."
    )]
    async fn panic_paths(
        &self,
        Parameters(params): Parameters<PanicPathsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "panic_paths",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_panic_paths::tool_panic_paths(self.ctx(), params),
        )
        .await
    }
}
