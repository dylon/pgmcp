//! Adaptive-tool-surface meta-tools: discovery (`tool_catalog`), dynamic
//! per-session expansion (`enable_tools` / `disable_tools`, each emitting a
//! `tools/list_changed` notification), and a generic dispatch fallback
//! (`call_tool`). The per-block router is composed in `server.rs` via
//! `assembled_tool_router()`.
//!
//! `tool_catalog` delegates to the `SystemContext`-only body in
//! `crate::mcp::tools::tool_meta` (so it is CLI-dispatchable + covered by the
//! dispatch-coverage gate). The stateful tools are implemented here because they
//! need `&McpServer`: the request peer (to emit `tools/list_changed`), the MCP
//! session id (the overlay key), and the name→body dispatch table (`call_tool`).
#![allow(clippy::too_many_lines)]

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};
use serde_json::json;

use crate::mcp::server::McpServer;
use crate::mcp::server::*;
use crate::mcp::tools::sota_helpers::json_result;

#[rmcp::tool_router(router = router_meta, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        name = "tool_catalog",
        description = "Browse/search this server's OWN MCP tools by a natural-language query and/or \
domain. USE WHEN: you need a capability that is not in your current tools/list — you start with a \
learned default working set, not the full ~330-tool catalog. Returns tool names + one-line \
descriptions; then `enable_tools` the ones you want (they appear natively) or `call_tool` to invoke \
one directly. DO NOT USE WHEN: searching indexed source code — use semantic_search / grep."
    )]
    async fn meta_tool_catalog(
        &self,
        Parameters(params): Parameters<ToolCatalogParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tool_catalog",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_meta::tool_tool_catalog(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Add tools to THIS session's tools/list so they appear natively. Provide exact \
`names`, a `domain` (all its tools), and/or a `query` (top semantic matches) — the union is enabled. \
The server then emits a tools/list_changed notification; re-fetch tools/list to see them. USE WHEN: \
tool_catalog surfaced a tool you want to use repeatedly. For a one-off call without enabling, use \
call_tool instead."
    )]
    async fn enable_tools(
        &self,
        Parameters(params): Parameters<EnableToolsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "enable_tools",
            30,
            &_ctx,
            &summarize_debug(&params),
            self.do_enable_tools(&_ctx, params),
        )
        .await
    }

    #[tool(
        description = "Remove tools you previously enable_tools-ed from THIS session (pass `names`, \
or `all:true` to reset to your learned defaults). Emits tools/list_changed. Tools in your mandatory \
core or learned defaults stay visible."
    )]
    async fn disable_tools(
        &self,
        Parameters(params): Parameters<DisableToolsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "disable_tools",
            30,
            &_ctx,
            &summarize_debug(&params),
            self.do_disable_tools(&_ctx, params),
        )
        .await
    }

    #[tool(
        name = "call_tool",
        description = "Invoke ANY tool by name with an args object, even one not in your tools/list. \
A direct fallback for reaching the long tail without enable_tools (useful if your client does not \
re-fetch tools/list on a list_changed notification). The call is attributed to the inner tool, so it \
also teaches your learned defaults."
    )]
    async fn meta_call_tool(
        &self,
        Parameters(params): Parameters<CallToolParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let inner = params.name.trim().to_string();
        if matches!(
            inner.as_str(),
            "call_tool" | "enable_tools" | "disable_tools"
        ) {
            return Err(McpError::invalid_params(
                format!("call_tool cannot invoke the meta-tool '{inner}'"),
                None,
            ));
        }
        self.dispatch_for_call_tool(&_ctx, &inner, params.args)
            .await
    }
}

// Non-`#[tool]` helpers (kept out of the `#[tool_router]` block so the macro only
// processes the four tool methods above).
impl McpServer {
    async fn do_enable_tools(
        &self,
        ctx: &RequestContext<RoleServer>,
        params: EnableToolsParams,
    ) -> Result<CallToolResult, McpError> {
        let Some(session) = extract_mcp_session_id(ctx) else {
            return Err(McpError::invalid_params(
                "enable_tools requires an MCP session (HTTP transport); it is not available on the \
CLI dispatch path",
                None,
            ));
        };
        let mut resolved =
            crate::mcp::tools::tool_meta::resolve_enable_targets(self.ctx(), &params).await?;
        if resolved.is_empty() {
            return Err(McpError::invalid_params(
                "enable_tools matched no tools — provide `names`, a `domain`, or a `query`",
                None,
            ));
        }
        resolved.sort();
        resolved.dedup();

        let sessions = self.ctx().tool_sessions();
        crate::mcp::tool_policy::prune_sessions(
            sessions,
            crate::mcp::tool_policy::MAX_TOOL_SESSIONS,
            crate::mcp::tool_policy::TOOL_SESSION_TTL_SECS,
        );
        // Mutate the overlay in a tight scope so the DashMap shard guard is
        // dropped BEFORE the await below (never hold a shard lock across await).
        let session_total = {
            let mut entry = sessions.entry(session.clone()).or_default();
            for name in &resolved {
                entry.enabled.insert(name.clone());
            }
            entry.enabled.len()
        };

        // Ask the client to re-fetch tools/list so the newly enabled tools appear
        // natively. Best-effort: a client that ignores list_changed can still reach
        // them via call_tool, and they surface on its next tools/list.
        if let Err(e) = ctx.peer.notify_tool_list_changed().await {
            tracing::debug!(error = %e, "notify_tool_list_changed failed (client may not support it)");
        }

        json_result(&json!({
            "enabled": resolved,
            "session_enabled_total": session_total,
            "note": "Now in your tools/list for this session. If your client did not refresh, invoke \
        them now via call_tool; they also appear on your next tools/list.",
        }))
    }

    async fn do_disable_tools(
        &self,
        ctx: &RequestContext<RoleServer>,
        params: DisableToolsParams,
    ) -> Result<CallToolResult, McpError> {
        let Some(session) = extract_mcp_session_id(ctx) else {
            return Err(McpError::invalid_params(
                "disable_tools requires an MCP session (HTTP transport)",
                None,
            ));
        };
        let sessions = self.ctx().tool_sessions();
        let (removed, remaining) = match sessions.get_mut(&session) {
            Some(mut entry) => {
                if params.all {
                    let n = entry.enabled.len();
                    entry.enabled.clear();
                    (n, 0usize)
                } else {
                    let mut removed = 0usize;
                    for name in &params.names {
                        if entry.enabled.remove(name.trim()) {
                            removed += 1;
                        }
                    }
                    (removed, entry.enabled.len())
                }
            }
            None => (0, 0),
        };
        if let Err(e) = ctx.peer.notify_tool_list_changed().await {
            tracing::debug!(error = %e, "notify_tool_list_changed failed (client may not support it)");
        }
        json_result(&json!({
            "removed": removed,
            "session_enabled_remaining": remaining,
        }))
    }
}
