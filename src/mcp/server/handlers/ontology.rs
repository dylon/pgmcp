//! `ontology_*` MCP tool handlers (Phase 6) — the client-facing surface of the
//! hierarchical ontology. Bodies live in `crate::mcp::tools::tool_ontology`; the
//! per-block router is composed in `server.rs` via `assembled_tool_router()`.
#![allow(clippy::too_many_lines)]

use crate::mcp::server::McpServer;
use crate::mcp::server::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};

#[rmcp::tool_router(router = router_ontology, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Show a project/workspace ontology hierarchy: per-facet concepts and their \
is_a/part_of/broader edges. Omit `facet` for all facets. Facets include architecture, component, \
algorithm, data_structure, paradigm, design_pattern, engineering_practice, strategy, security, \
concurrency, protocol, domain_concept, invariant, tool, system, resource, collection."
    )]
    async fn ontology_tree(
        &self,
        Parameters(params): Parameters<OntologyTreeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "ontology_tree",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_ontology::tool_ontology_tree(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Inspect one ontology concept (by name or id): facet, curation status, \
invariant constraint/rationale, and evidence pointers."
    )]
    async fn ontology_concept(
        &self,
        Parameters(params): Parameters<OntologyConceptParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "ontology_concept",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_ontology::tool_ontology_concept(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Search ontology concepts by name substring (optional facet filter).")]
    async fn ontology_search(
        &self,
        Parameters(params): Parameters<OntologySearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "ontology_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_ontology::tool_ontology_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Surface the design invariants/constraints governing a file BEFORE you edit it \
(constraint + rationale + evidence). Use this to avoid violating project design intent — e.g. \
'ambiguity must propagate end-to-end; never disambiguate prematurely over the parse tree'."
    )]
    async fn ontology_invariants_for_file(
        &self,
        Parameters(params): Parameters<OntologyInvariantsForFileParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "ontology_invariants_for_file",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_ontology::tool_ontology_invariants_for_file(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Assert a design invariant (a rule future edits must respect). Agent-authored \
invariants are recorded as `candidate` only — a human curator promotes them to canonical."
    )]
    async fn ontology_assert_invariant(
        &self,
        Parameters(params): Parameters<OntologyAssertInvariantParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "ontology_assert_invariant",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_ontology::tool_ontology_assert_invariant(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Create an ontology concept of a given facet (e.g. a `collection` category like \
'Formal Verification Systems', or a `tool`/`system`/`resource`). Agent-authored ⇒ candidate."
    )]
    async fn ontology_create_concept(
        &self,
        Parameters(params): Parameters<OntologyCreateConceptParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "ontology_create_concept",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_ontology::tool_ontology_create_concept(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Relate two concepts: is_a (subsumption), part_of (mereology), \
broader/narrower (SKOS), or member_of (instance → collection)."
    )]
    async fn ontology_link(
        &self,
        Parameters(params): Parameters<OntologyLinkParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "ontology_link",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_ontology::tool_ontology_link(self.ctx(), params),
        )
        .await
    }
}
