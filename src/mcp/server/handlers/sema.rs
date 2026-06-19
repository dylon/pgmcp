//! Semantic-shape, type & paradigm handlers.
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

#[rmcp::tool_router(router = router_sema, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Find functions in different languages with matching signature shape. \
USE WHEN: validating cross-language ports (MeTTa→Rholang→Rust), auditing whether a compiler \
preserved semantics, or harmonizing APIs across language SDKs. \
Reads from the materialized `cross_language_signature_clones` table — call `trigger_cron` \
with `cross_language_signatures` to refresh."
    )]
    async fn cross_language_api_equivalents(
        &self,
        Parameters(params): Parameters<CrossLanguageApiEquivalentsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "cross_language_api_equivalents",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_cross_language_api_equivalents::tool_cross_language_api_equivalents(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Read-only LSP-shaped query over the indexed symbol graph (ADR-026). One \
`op` selects the operation: document_symbol | workspace_symbol | definition | references | hover | \
type_definition | implementation | call_hierarchy_incoming | call_hierarchy_outgoing | \
type_hierarchy_super | type_hierarchy_sub | folding_range | signature_help | document_highlight | \
capabilities. Params {project, op, file_path?, symbol?, scope?, limit}. USE WHEN you want \
go-to-definition / find-references / call-hierarchy / hover without opening files. No mutation. Call \
op=capabilities to list ops + their backing data."
    )]
    async fn lsp_query(
        &self,
        Parameters(params): Parameters<LspQueryParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "lsp_query",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_lsp_query::tool_lsp_query(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Search for functions by structural type shape: return type tags, \
parameter type tags, effects. USE WHEN: 'find async functions returning Result<T,_>', \
'all handlers taking Request<_>', 'database-touching functions in module foo'. \
Backed by GIN-indexed `return_type_tags` and `symbol_parameters.type_tags`."
    )]
    async fn type_shape_search(
        &self,
        Parameters(params): Parameters<TypeShapeSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "type_shape_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_type_shape_search::tool_type_shape_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Find call sites of a target path, filtered by the caller's signature \
shape (e.g. 'callers whose parameter N has type-tag Mutex'). USE WHEN: scoping \
a refactor that touches all callers carrying a specific type, or locating \
callers in a specific effect-set context."
    )]
    async fn find_callers_by_signature(
        &self,
        Parameters(params): Parameters<FindCallersBySignatureParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "find_callers_by_signature",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_find_callers_by_signature::tool_find_callers_by_signature(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Forward or reverse effect closure along resolved call edges. \
Forward (seed_symbol_id): which effects are reached from this symbol? \
Reverse (target_effects): which symbols reach any of these effects? \
USE WHEN: tracing 'what touches network?', 'who could reach gpu_kernel?', \
or 'what does this entry point ultimately do?'."
    )]
    async fn effect_propagation(
        &self,
        Parameters(params): Parameters<EffectPropagationParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "effect_propagation",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_effect_propagation::tool_effect_propagation(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Temporal effect-drift: which symbols recently GAINED or LOST a shadow-ASR \
effect (unsafe / async / may_panic / blocking_io / …) over time. USE WHEN: 'what just became \
unsafe?', auditing when async crept into a module, or tracking semantic API-contract changes. \
Reads the append-only symbol_effect_history ledger the symbol-extraction cron maintains by \
diffing each file's freshly-extracted effect set against the prior one. Filter by project / \
effect / change (gained|lost) / since_days; newest first."
    )]
    async fn effect_drift(
        &self,
        Parameters(params): Parameters<EffectDriftParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "effect_drift",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_effect_drift::tool_effect_drift(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "List the type-tag and effect vocabularies with per-tag usage counts \
and descriptions. USE WHEN: orienting to the tag schema before formulating \
queries, or auditing which tags actually appear in this project's code."
    )]
    async fn type_tag_dictionary(
        &self,
        Parameters(params): Parameters<TypeTagDictionaryParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "type_tag_dictionary",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_type_tag_dictionary::tool_type_tag_dictionary(
                self.ctx(),
                params,
            ),
        )
        .await
    }
}
