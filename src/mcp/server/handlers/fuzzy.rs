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
    #[tool(description = "Normalize a term via liblevenshtein's phonetic framework.")]
    async fn phonetic_normalize(
        &self,
        Parameters(params): Parameters<PhoneticNormalizeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap_with_project(
            self.stats(),
            "phonetic_normalize",
            5,
            &_ctx,
            &summarize_debug(&params),
            params.project.clone(),
            crate::mcp::tools::tool_phonetic_normalize::run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Reverse-expand a query into a phonetic regex (e.g. nite → (n|kn)i(t|te|ght))."
    )]
    async fn expand_query_to_phonetic_pattern(
        &self,
        Parameters(params): Parameters<ExpandQueryToPhoneticPatternParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap_with_project(
            self.stats(),
            "expand_query_to_phonetic_pattern",
            5,
            &_ctx,
            &summarize_debug(&params),
            params.project.clone(),
            crate::mcp::tools::tool_expand_query_to_phonetic_pattern::run(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Articulatory edit distance between two strings (IPA-feature weighted).")]
    async fn articulatory_distance(
        &self,
        Parameters(params): Parameters<ArticulatoryDistanceParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "articulatory_distance",
            2,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_articulatory_distance::run(self.ctx(), params),
        )
        .await
    }

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

    #[tool(description = "Substring search via suffix automaton (exact, fast).")]
    async fn substring_search(
        &self,
        Parameters(params): Parameters<SubstringSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "substring_search",
            5,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_substring_search::run(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Per-token fuzzy grep via liblevenshtein's TokenGrep semantics.")]
    async fn token_grep(
        &self,
        Parameters(params): Parameters<TokenGrepParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "token_grep",
            5,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_token_grep::run(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Time-series fuzzy match via MSM distance (commit-cadence patterns).")]
    async fn time_series_fuzzy_match(
        &self,
        Parameters(params): Parameters<TimeSeriesFuzzyMatchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "time_series_fuzzy_match",
            5,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_time_series_fuzzy_match::run(self.ctx(), params),
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

    #[tool(description = "In-process mandate dedup via Damerau-Levenshtein over an active set.")]
    async fn mandate_dedup_v2(
        &self,
        Parameters(params): Parameters<MandateDedupV2Params>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "mandate_dedup_v2",
            5,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_mandate_dedup_v2::run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Positional fuzzy grep over a caller-supplied haystack via liblevenshtein TokenGrep: matches the query approximately at every position (byte spans + edit distance), not as whole-line dictionary terms. For approximate search across the INDEXED codebase, use `grep` with fuzzy=true."
    )]
    async fn fuzzy_grep(
        &self,
        Parameters(params): Parameters<FuzzyGrepParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "fuzzy_grep",
            5,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_fuzzy_grep::run(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Phonetic grep over comment/string lines (PhoneticGrepOnline).")]
    async fn phonetic_grep_comments(
        &self,
        Parameters(params): Parameters<PhoneticGrepCommentsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap_with_project(
            self.stats(),
            "phonetic_grep_comments",
            5,
            &_ctx,
            &summarize_debug(&params),
            params.project.clone(),
            crate::mcp::tools::tool_phonetic_grep_comments::run(self.ctx(), params),
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
        description = "Phonetic naming consistency: identifiers that share a phonetic class but differ in spelling."
    )]
    async fn phonetic_naming_consistency(
        &self,
        Parameters(params): Parameters<PhoneticNamingConsistencyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "phonetic_naming_consistency",
            5,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_phonetic_naming_consistency::run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Articulatory naming consistency: identifiers within an articulatory-distance threshold."
    )]
    async fn articulatory_naming_consistency(
        &self,
        Parameters(params): Parameters<ArticulatoryNamingConsistencyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "articulatory_naming_consistency",
            5,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_articulatory_naming_consistency::run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Rename oracle: pick the most-likely current-day rename for a removed symbol."
    )]
    async fn rename_oracle(
        &self,
        Parameters(params): Parameters<RenameOracleParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "rename_oracle",
            5,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_rename_oracle::run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Semantic-issue scan over a Code Property Graph via libgrammstein's \
GnnSemanticScorer. Returns SemanticIssue records (unused bindings, suspicious DFG \
patterns, etc.) detected by the GNN's heuristic-edge-walk detection — no neural \
inference dependency required (the `code-neural` feature gates a placeholder \
inference path that the current upstream heuristic detector doesn't use)."
    )]
    async fn gnn_semantic_issues(
        &self,
        Parameters(params): Parameters<GnnSemanticIssuesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "gnn_semantic_issues",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_gnn_semantic_issues::run(self.ctx(), params),
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
