//! The **tape verbs** — the agent-facing MCP surface over the context-tape
//! paging substrate (Phase 4).
//!
//! These nine `#[tool]` methods are the **black-box-legal** addressable surface
//! any agent (Claude, Codex, …) may call against a recursion tree's tape: they
//! are *analytical* (NO shell, NO code execution), they NEVER write the user's
//! source files, and the durable corpus is READ-ONLY (reads may hydrate from it;
//! writes target the per-tree `TapeStore` via the `TapeRegistry`). Every verb is
//! scoped to a required `tree` id (`RlmFrame.root_task_id`, or a fresh UUID for
//! standalone use) so two concurrent runs never collide in the backing store.
//!
//! Each method is a one-line forward (via `instrumented_tool_wrap`) into the
//! corresponding `tool_tape_*` body in `crate::mcp::tools`; the per-block router
//! `router_tape` is summed into `assembled_tool_router()` in `server.rs`.
#![allow(clippy::too_many_lines)]

use crate::mcp::server::McpServer;
use crate::mcp::server::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};

#[rmcp::tool_router(router = router_tape, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "tape_get — fetch one tape page's situated bytes for a recursion tree: resident \
hot/out-of-core cascade, else hydrate the READ-ONLY corpus and admit it clean. Returns \
{address, content, dirty}. Black-box-legal: analytical, no shell/exec; never writes the user's \
files; corpus is read-only."
    )]
    async fn tape_get(
        &self,
        Parameters(params): Parameters<TapeGetParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tape_get",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_tape_get::tool_tape_get(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "tape_put — stage page content into the per-tree tape store as DIRTY (omit \
`address` to mint a fresh tree-local Scratch slot). Returns {address, dirty:true}. Write-back \
promotion into durable memory is doubly gated (caller promote=true AND daemon allow_promotion AND \
an existing observation address); the corpus is never a promotion target. Black-box-legal: \
analytical, no shell/exec; NEVER writes the user's source files."
    )]
    async fn tape_put(
        &self,
        Parameters(params): Parameters<TapePutParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tape_put",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_tape_put::tool_tape_put(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "tape_peek — a cheap head/size probe over a tape page WITHOUT materializing its \
full content: returns a bounded head preview (default 256 bytes), size_bytes, and n_pages (resident \
pages under the address's path prefix). Resident-only; never hydrates. Black-box-legal: analytical, \
no shell/exec; reads only; never writes the user's files."
    )]
    async fn tape_peek(
        &self,
        Parameters(params): Parameters<TapePeekParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tape_peek",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_tape_peek::tool_tape_peek(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "tape_slice — positional range scan over the per-tree tape store, in address \
(key) order, between two address paths [lo, hi]. Returns {pages:[{address, content}], truncated}. \
Black-box-legal: analytical, no shell/exec; reads only (resident pages); never writes the user's \
files."
    )]
    async fn tape_slice(
        &self,
        Parameters(params): Parameters<TapeSliceParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tape_slice",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_tape_slice::tool_tape_slice(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "tape_grep — substring search over the tape: scope='tree' (default) uses the \
per-tree store's substring index over resident content; scope='corpus' resolves matching chunks \
from the READ-ONLY corpus; scope='both' unions them. Black-box-legal: analytical, no shell/exec; \
corpus reads are read-only; never writes the user's files."
    )]
    async fn tape_grep(
        &self,
        Parameters(params): Parameters<TapeGrepParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tape_grep",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_tape_grep::tool_tape_grep(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "tape_fuzzy — Levenshtein fuzzy-path search over the per-tree store: returns \
resident page addresses whose path is within max_distance (default 2) edits of the query, ordered \
by ascending distance → {hits:[{address, distance}]}. Black-box-legal: analytical, no shell/exec; \
reads only; never writes the user's files."
    )]
    async fn tape_fuzzy(
        &self,
        Parameters(params): Parameters<TapeFuzzyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tape_fuzzy",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_tape_fuzzy::tool_tape_fuzzy(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "tape_semantic — top-k semantic retrieval over the READ-ONLY corpus: embeds the \
natural-language query host-side and returns nearest chunk references → {hits:[{address, \
similarity}]}. Requires a live database; unavailable in CLI/mock-DB mode (returns no hits). \
Black-box-legal: analytical, no shell/exec; corpus reads are read-only; never writes the user's \
files."
    )]
    async fn tape_semantic(
        &self,
        Parameters(params): Parameters<TapeSemanticParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tape_semantic",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_tape_semantic::tool_tape_semantic(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "tape_list — enumerate resident page addresses in the per-tree store (optionally \
under a path prefix), in address order, capped by limit → {addresses, dirty_count}. Black-box-legal: \
analytical, no shell/exec; reads only; never writes the user's files."
    )]
    async fn tape_list(
        &self,
        Parameters(params): Parameters<TapeListParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tape_list",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_tape_list::tool_tape_list(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "tape_stat — residency statistics for the per-tree tape store: resident_bytes, \
n_pages, n_dirty, and n_ooc_segments (out-of-core overlay). Black-box-legal: analytical, no \
shell/exec; reads only; never writes the user's files."
    )]
    async fn tape_stat(
        &self,
        Parameters(params): Parameters<TapeStatParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tape_stat",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_tape_stat::tool_tape_stat(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "tape_repl — run a sandboxed white-box REPL script against a recursion tree's \
tape (the white-box / latent tier; the deny-by-default rhai engine exposes ONLY the nine tape verbs, \
no shell/fs/net, eval disabled), under hard deterministic limits. Gated: admitted ONLY for a white-box \
caller (a black-box agent is structurally refused; white-box status is a host-side fact, never a \
self-reported claim) AND when `experiment_slug` resolves to an Open experiment. Returns \
{admitted:false, reason} on refusal, else {admitted:true, value, value_type, pages_touched, \
bytes_touched, ops, over_limit, limit, error} (a budget abort is over_limit:true, not an error). \
Analytical wrapper: pgmcp runs no shell/exec; the durable corpus is never written (put is \
Scratch-only); never writes the user's source files."
    )]
    async fn tape_repl(
        &self,
        Parameters(params): Parameters<TapeReplParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // The REPL admission gate is the white-box / latent-tier trust boundary: a
        // black-box agent (Claude, Codex) must be structurally refused. Derive the
        // caller's transport identity host-side from the `initialize` handshake
        // (`clientInfo.name`, lowercased) — NEVER from the request payload — and
        // thread it into the body. `extract_caller` yields `"unknown"` when the peer
        // has not completed `initialize`, which `repl_host::caller_role` treats as
        // black-box (fail-closed). The body is responsible for the role mapping; we
        // pass the raw identity string.
        let caller_identity = extract_caller(&_ctx).client_name;
        instrumented_tool_wrap(
            self.stats(),
            "tape_repl",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_tape_repl::tool_tape_repl_with_caller(
                self.ctx(),
                params,
                Some(&caller_identity),
            ),
        )
        .await
    }
}
