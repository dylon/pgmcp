//! Memory-server CRUD tool handlers (official-compat entities/relations/observations).
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

#[rmcp::tool_router(router = router_memory_crud, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Memory-server: create entities (knowledge-graph nodes). Drop-in compatible \
with @modelcontextprotocol/server-memory's `create_entities`. Idempotent on \
(name, entity_type): re-use the active row and append observations. Optional \
`scope` attaches the entities to a (user_id, agent_id, session_id, project_id) \
tuple — defaults to workspace-wide."
    )]
    async fn memory_create_entities(
        &self,
        Parameters(params): Parameters<MemoryCreateEntitiesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_create_entities",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_crud::tool_memory_create_entities(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: create directed typed relations between existing entities. \
Drop-in compatible with @modelcontextprotocol/server-memory's `create_relations`. \
Each input `{from, to, relation_type}` is resolved against active entities by \
name; unresolved endpoints return id=-1 in the response."
    )]
    async fn memory_create_relations(
        &self,
        Parameters(params): Parameters<MemoryCreateRelationsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_create_relations",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_crud::tool_memory_create_relations(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: append observations to existing entities. Drop-in \
compatible with @modelcontextprotocol/server-memory's `add_observations`. \
Observations are content-deduped per entity (content_sha256 UNIQUE)."
    )]
    async fn memory_add_observations(
        &self,
        Parameters(params): Parameters<MemoryAddObservationsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_add_observations",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_crud::tool_memory_add_observations(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: soft-delete entities by name (sets valid_to = NOW()). \
Bi-temporal: deleted rows remain queryable via `memory_facts_at(t < deletion_time)`. \
Drop-in compatible with @modelcontextprotocol/server-memory's `delete_entities`."
    )]
    async fn memory_delete_entities(
        &self,
        Parameters(params): Parameters<MemoryDeleteEntitiesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_delete_entities",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_crud::tool_memory_delete_entities(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: soft-delete observations by content text under named \
entities. Drop-in compatible with @modelcontextprotocol/server-memory's \
`delete_observations`."
    )]
    async fn memory_delete_observations(
        &self,
        Parameters(params): Parameters<MemoryDeleteObservationsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_delete_observations",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_crud::tool_memory_delete_observations(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Memory-server: soft-delete relations by (from, to, relation_type). \
Drop-in compatible with @modelcontextprotocol/server-memory's `delete_relations`."
    )]
    async fn memory_delete_relations(
        &self,
        Parameters(params): Parameters<MemoryDeleteRelationsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_delete_relations",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_crud::tool_memory_delete_relations(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: dump the active knowledge graph (entities + observations + \
relations) under an optional scope. Capped by limit_entities (default 200, max 2000). \
Drop-in compatible with @modelcontextprotocol/server-memory's `read_graph`."
    )]
    async fn memory_read_graph(
        &self,
        Parameters(params): Parameters<MemoryReadGraphParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_read_graph",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_crud::tool_memory_read_graph(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: ILIKE substring search across entity name/type/canonical_name \
and observation content. The Phase 3.1 baseline matching the official server's \
`search_nodes`; the pgmcp-extension `memory_semantic_search` (Phase 3.2, lands \
with BGE-M3 cutover) adds vector similarity."
    )]
    async fn memory_search_nodes(
        &self,
        Parameters(params): Parameters<MemorySearchNodesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_search_nodes",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_crud::tool_memory_search_nodes(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: open named entities — returns each entity plus its active \
observations and incoming/outgoing relations. Drop-in compatible with \
@modelcontextprotocol/server-memory's `open_nodes`."
    )]
    async fn memory_open_nodes(
        &self,
        Parameters(params): Parameters<MemoryOpenNodesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_open_nodes",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_memory_crud::tool_memory_open_nodes(self.ctx(), params),
        )
        .await
    }
}
