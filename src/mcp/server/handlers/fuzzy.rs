//! Fuzzy, phonetic & articulatory search handlers.
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

#[rmcp::tool_router(router = router_fuzzy, vis = "pub(crate)")]
impl McpServer {
    #[tool(description = "Read the persisted dendrogram-topic-hierarchy for a project (Phase 7).")]
    async fn dendrogram_topic_hierarchy(
        &self,
        Parameters(params): Parameters<DendrogramTopicHierarchyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap_with_project(
            self.stats(),
            "dendrogram_topic_hierarchy",
            10,
            &_ctx,
            &summarize_debug(&params),
            Some(params.project.clone()),
            crate::mcp::tools::tool_dendrogram_topic_hierarchy::run(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Fuzzy symbol search via Damerau-Levenshtein over a candidate set.")]
    async fn fuzzy_symbol_search(
        &self,
        Parameters(params): Parameters<FuzzySymbolSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap_with_project(
            self.stats(),
            "fuzzy_symbol_search",
            5,
            &_ctx,
            &summarize_debug(&params),
            Some(params.project.clone()),
            crate::mcp::tools::tool_fuzzy_symbol_search::run(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Fuzzy path search via Damerau-Levenshtein over indexed file paths.")]
    async fn fuzzy_path_search(
        &self,
        Parameters(params): Parameters<FuzzyPathSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap_with_project(
            self.stats(),
            "fuzzy_path_search",
            5,
            &_ctx,
            &summarize_debug(&params),
            Some(params.project.clone()),
            crate::mcp::tools::tool_fuzzy_path_search::run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Correct a user query against a project's persistent symbol vocabulary using pgmcp's WFST corrector: per-token Damerau-Levenshtein candidates from the symbol trie, an edit+phonetic-cost correction lattice, and (when a trained per-project model exists) Modified-Kneser-Ney LM rescoring. Returns corrected text, changed flag, confidence, and used_lm. Params: query, project, max_distance (default 2), lm_weight (default 0.5)."
    )]
    async fn correct_query(
        &self,
        Parameters(params): Parameters<CorrectQueryParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap_with_project(
            self.stats(),
            "correct_query",
            30,
            &_ctx,
            &summarize_debug(&params),
            Some(params.project.clone()),
            crate::mcp::tools::tool_correct_query::run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Composed phonetic∘edit symbol search over a project's persistent symbol trie (liblevenshtein PhoneticNormalizedDictionary): normalizes query+terms with the active phonetic rules, matches within max_distance in normalized space, returns symbols with kind/visibility/file_id/line ranked by edit then articulatory distance. Params: query, project, max_distance (default 2), limit (default 20)."
    )]
    async fn phonetic_symbol_search(
        &self,
        Parameters(params): Parameters<PhoneticSymbolSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap_with_project(
            self.stats(),
            "phonetic_symbol_search",
            5,
            &_ctx,
            &summarize_debug(&params),
            Some(params.project.clone()),
            crate::mcp::tools::tool_phonetic_symbol_search::run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Lint same-shape APIs for signature smells: long parameter lists, \
primitive obsession, boolean-flag explosion, inconsistent parameter naming across \
functions sharing the same shape. Returns categorized findings; consumers can act \
on each category independently."
    )]
    async fn signature_lint(
        &self,
        Parameters(params): Parameters<SignatureLintParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap_with_project(
            self.stats(),
            "signature_lint",
            30,
            &_ctx,
            &summarize_debug(&params),
            Some(params.project.clone()),
            crate::mcp::tools::tool_signature_lint::tool_signature_lint(self.ctx(), params),
        )
        .await
    }
}
