//! Core search & top-level read handlers (search, grep, orient, mandates).
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

#[rmcp::tool_router(router = router_core, vis = "pub(crate)")]
impl McpServer {
    #[tool(description = "Vector-similarity search across all indexed files. \
USE WHEN: query is conceptual ('error handling patterns', 'auth flow', 'how does X work'), \
cross-project, or you don't know the exact tokens to search for. \
DO NOT USE WHEN: you have an exact symbol/string and just need its locations — `grep` or \
the built-in `Grep` is faster. \
Filter by project name to scope results. Use project: \"claude\" to search past Claude \
Code session transcripts, memory files, and plans from ~/.claude/.")]
    async fn semantic_search(
        &self,
        Parameters(params): Parameters<SemanticSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "semantic_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_semantic_search::tool_semantic_search(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "PostgreSQL full-text search across all indexed files. \
USE WHEN: searching for exact keywords or phrases across multiple projects, with \
ranking by relevance. \
DO NOT USE WHEN: you only need to search the current cwd (built-in `Grep` is faster), \
or when the query is conceptual rather than lexical (use `semantic_search` instead). \
Filter by project; use project: \"claude\" to search Claude Code session transcripts.")]
    async fn text_search(
        &self,
        Parameters(params): Parameters<TextSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "text_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_text_search::tool_text_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Regex pattern search across all indexed files (PostgreSQL ~ operator). \
USE WHEN: searching for a regex across the full indexed codebase or across multiple \
projects, especially when the model has no idea which project the match is in. \
DO NOT USE WHEN: you only need to search within the current cwd or a specific small \
directory tree — the built-in `Grep` tool is faster and respects .gitignore. \
Returns file paths, line numbers, and matching snippets across all indexed projects. \
Set fuzzy=true to match the pattern APPROXIMATELY (liblevenshtein TokenGrep over indexed \
chunks) — finds typo'd / near-miss identifiers exact regex would miss; bound the scan with project."
    )]
    async fn grep(
        &self,
        Parameters(params): Parameters<GrepParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "grep",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_grep::tool_grep(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Read an indexed file by absolute path, returning its content along with \
indexing metadata. \
USE WHEN: reading a file that is part of an indexed project AND you want the metadata \
envelope (last_indexed_at, language, chunk count). \
DO NOT USE WHEN: reading a file you just wrote this turn (not yet indexed), reading a \
.gitignore'd file, or reading a file outside the indexed workspaces — use the built-in \
`Read` tool for those."
    )]
    async fn read_file(
        &self,
        Parameters(params): Parameters<ReadFileParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "read_file",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_read_file::tool_read_file(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "List all discovered projects with file counts.")]
    async fn list_projects(
        &self,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "list_projects",
            30,
            &_ctx,
            "",
            crate::mcp::tools::tool_list_projects::tool_list_projects(self.ctx()),
        )
        .await
    }

    #[tool(
        description = "Composite first-step orientation snapshot for a project. Bundles project metadata, language breakdown, depth-2 directory tree, key entry points (top files by PageRank), recently-changed files, and top topics into one call. USE WHEN: entering an unfamiliar codebase or starting a non-trivial task — call this before scattering across list_projects/project_tree/centrality_analysis. Returns a `health` envelope flagging stale graph metrics or missing topic data so you can interpret partial results correctly."
    )]
    async fn orient(
        &self,
        Parameters(params): Parameters<OrientParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "orient",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_orient::tool_orient(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Return the effective workspace/project mandate bundle from existing AGENTS.md, CLAUDE.md, and project .pgmcp.toml sources. USE WHEN: starting non-trivial work, checking project rules, or wiring client hooks. MCP surfaces this context advisory-only; hard enforcement still belongs in client hooks, pre-push hooks, CI, or verification scripts. If `session_id` is supplied, the response also includes any active session-scoped mandates and durable mandates promoted for the resolved project."
    )]
    async fn mandate_context(
        &self,
        Parameters(params): Parameters<MandateContextParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "mandate_context",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_mandate_context::tool_mandate_context(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 0: vector-similarity search over historical user \
prompts captured in `session_prompts`. USE WHEN: you want to recall what the user has \
previously asked across sessions ('what have I said about X'), useful for grounding \
agent responses in prior context. Optionally filter by project name or session UUID. \
Returns the top-k most similar prompts with their session id, timestamp, and similarity \
score."
    )]
    async fn recall_prompts(
        &self,
        Parameters(params): Parameters<RecallPromptsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "recall_prompts",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_recall_prompts::tool_recall_prompts(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 0: full-text search over `durable_mandates` \
(promoted standing directives). USE WHEN: you want to look up project rules or \
preferences by keyword. Filters: polarity (always/never/prefer/avoid/...), scope \
('project' or 'workspace'), project_id (workspace-scoped rows are returned regardless). \
Returns mandates ranked by Postgres full-text relevance, then by promotion recency."
    )]
    async fn search_mandates(
        &self,
        Parameters(params): Parameters<SearchMandatesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "search_mandates",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_search_mandates::tool_search_mandates(self.ctx(), params),
        )
        .await
    }
}
