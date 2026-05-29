//! Memory-server search, graph-RAG, forget & session-mandate handlers.
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

#[rmcp::tool_router(router = router_memory_search, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Memory-server: BGE-M3 vector search over memory_observations (scope/tier \
filtered) — vector similarity ONLY. Use `memory_hybrid_search` to also match keywords, or \
`memory_unified_search` to search entities/chunks/topics/mandates/commits too. The pgmcp extension \
to the official-compat `memory_search_nodes`."
    )]
    async fn memory_semantic_search(
        &self,
        Parameters(params): Parameters<MemorySemanticSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_semantic_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_ext::tool_memory_semantic_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: hybrid search over memory_observations — RRF fusion of \
Postgres FTS and BGE-M3 vector cosine (scope/tier filtered). Use WHEN both keywords and concepts \
matter; for the broader heterogeneous graph use `memory_unified_search`."
    )]
    async fn memory_hybrid_search(
        &self,
        Parameters(params): Parameters<MemoryHybridSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_hybrid_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_ext::tool_memory_hybrid_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: bi-temporal point-in-time snapshot. Returns the entities, \
observations, and relations that were active at `as_of` (RFC3339; defaults to NOW())."
    )]
    async fn memory_facts_at(
        &self,
        Parameters(params): Parameters<MemoryFactsAtParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_facts_at",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_ext::tool_memory_facts_at(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: depth-bounded BFS over memory_relations starting from \
one or more seed entity ids. Capped by max_depth (1..=6, default 2) and max_nodes (default \
200, max 1000)."
    )]
    async fn memory_relations_traverse(
        &self,
        Parameters(params): Parameters<MemoryRelationsTraverseParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_relations_traverse",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_ext::tool_memory_relations_traverse(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: anchor an entity to a (file | chunk | topic) with a typed \
anchor_type ('implements', 'tested-by', 'documented-in', 'caused-by', 'applies-to', ...). \
At least one of file_id, chunk_id, topic_id must be provided."
    )]
    async fn memory_anchor_entity(
        &self,
        Parameters(params): Parameters<MemoryAnchorEntityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_anchor_entity",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_ext::tool_memory_anchor_entity(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Memory-server: delete a code anchor by id.")]
    async fn memory_unanchor_entity(
        &self,
        Parameters(params): Parameters<MemoryUnanchorEntityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_unanchor_entity",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_ext::tool_memory_unanchor_entity(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: list code anchors for an entity, optionally filtered by \
anchor_type."
    )]
    async fn memory_find_code_for_entity(
        &self,
        Parameters(params): Parameters<MemoryFindCodeForEntityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_find_code_for_entity",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_ext::tool_memory_find_code_for_entity(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Memory-server: reverse lookup — entities anchored to a code object. \
Pass exactly one of file_id, chunk_id, topic_id."
    )]
    async fn memory_find_entities_for_code(
        &self,
        Parameters(params): Parameters<MemoryFindEntitiesForCodeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_find_entities_for_code",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_ext::tool_memory_find_entities_for_code(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Use WHEN you need prior project knowledge before acting — START HERE for \
memory retrieval: vector search over the heterogeneous unified-nodes view (memory_entity / \
observation / chunk / topic / durable_mandate / commit; optionally filter node_types). Narrower \
alternatives: `memory_semantic_search` (observations, vector only), `memory_hybrid_search` \
(observations, vector + keyword)."
    )]
    async fn memory_unified_search(
        &self,
        Parameters(params): Parameters<MemoryUnifiedSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_unified_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_graph_rag::tool_memory_unified_search(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 6.3: BFS over the heterogeneous unified-edge view. \
Returns reachable nodes and the edges that connect them, capped by depth ≤ 4 and \
max_nodes ≤ 500."
    )]
    async fn memory_neighbors(
        &self,
        Parameters(params): Parameters<MemoryNeighborsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_neighbors",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_graph_rag::tool_memory_neighbors(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Unified knowledge-graph BFS by *friendly* node reference. Accepts \
'<type>:<key>' where key is a natural id (file path, project/topic name, work_item public_id, \
experiment slug, commit sha, symbol name, agent id) or a numeric pk; resolves it and traverses \
the heterogeneous graph (depth ≤ 4, max_nodes ≤ 500). Valid types: file, project, work_item, \
experiment, topic, symbol, commit, agent, chunk, observation, memory_entity."
    )]
    async fn graph_neighbors(
        &self,
        Parameters(params): Parameters<GraphNeighborsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "graph_neighbors",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_graph_rag::tool_graph_neighbors(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 6.4: PathRAG-style path retrieval. Embed the query, \
seed top-k unified nodes, BFS-expand within max_hops, score paths, then flow-prune \
paths whose Jaccard overlap with a kept path exceeds prune_jaccard."
    )]
    async fn memory_path_search(
        &self,
        Parameters(params): Parameters<MemoryPathSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_path_search",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_graph_rag::tool_memory_path_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 6.2: HippoRAG-style Personalized PageRank over \
memory_relations. Seeds are the top-k entities by best-observation cosine; PPR runs \
25 iterations with the given alpha (teleport probability)."
    )]
    async fn memory_ppr_search(
        &self,
        Parameters(params): Parameters<MemoryPprSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_ppr_search",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_graph_rag::tool_memory_ppr_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 6.1: RAPTOR summary-tree query. Returns top-k summary \
nodes by cosine over summary_embedding, optionally filtered by tree level."
    )]
    async fn memory_raptor_search(
        &self,
        Parameters(params): Parameters<MemoryRaptorSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_raptor_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_graph_rag::tool_memory_raptor_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 10: resolve or list pgmcp client profiles. Pass \
`client_name` to see how pgmcp will format responses for that client (output_format, \
default_brief, include_provenance, per-tool description_overrides); pass `list_all=true` to \
see every profile pgmcp knows about. Built-in defaults for claude-code, codex, and \
generic ship with the binary; assets/client_profiles.toml overrides them."
    )]
    async fn pgmcp_client_profile(
        &self,
        Parameters(params): Parameters<PgmcpClientProfileParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "pgmcp_client_profile",
            10,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_client_profile::tool_pgmcp_client_profile(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 8.4: forget an entity / observation / relation. \
cascade=false (default) sets valid_to = NOW() (soft delete, queryable via \
memory_facts_at); cascade=true hard-deletes + every dependent FK row and writes an \
audit manifest to memory_forget_log."
    )]
    async fn memory_forget(
        &self,
        Parameters(params): Parameters<MemoryForgetParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_forget",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_forget::tool_memory_forget(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 8.2: report (dry_run=true, default) or perform \
(dry_run=false) the retention purge — hard-deletes soft-deleted, past-window, \
low-importance, non-superseded memory_* rows. Defaults pulled from \
[memory.retention] when not provided."
    )]
    async fn memory_purge_expired(
        &self,
        Parameters(params): Parameters<MemoryPurgeExpiredParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_purge_expired",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_forget::tool_memory_purge_expired(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 5: reflection. Pull recent observations from the given \
scope (or workspace-wide), call the LLM extractor's reflect path, persist higher-order \
observations with source='reflection' and derived_from = [obs_ids]. Refuses if the \
extractor is disabled or `[memory.reflection] agent_enabled = false`."
    )]
    async fn memory_reflect(
        &self,
        Parameters(params): Parameters<MemoryReflectParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_reflect",
            // Reflection involves an LLM call; allow up to 120 s before the
            // wrapper times out. The cron path runs without this wrapper.
            120,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_reflect::tool_memory_reflect(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "List session-scoped mandates extracted from prompts via the UserPromptSubmit hook. Provide either session_id (preferred) or cwd. Returns active mandates by default; pass status='all' for history."
    )]
    async fn session_mandates(
        &self,
        Parameters(params): Parameters<SessionMandatesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "session_mandates",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_session_mandates::tool_session_mandates(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Promote a session_mandates row to durable scope. scope='project' requires project_id. Inserts into durable_mandates; if write_to_file=true and target_file is supplied, appends the imperative under a marker section in that file (idempotent)."
    )]
    async fn promote_session_mandate(
        &self,
        Parameters(params): Parameters<PromoteSessionMandateParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "promote_session_mandate",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_session_mandates::tool_promote_session_mandate(
                self.ctx(),
                params,
            ),
        )
        .await
    }
}
