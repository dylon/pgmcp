//! `documentation_guidelines` — enumerate the documentation-authoring guidelines
//! pgmcp enforces across ALL agents.
//!
//! A nullary, DB-free tool: it returns the canonical static list from
//! `crate::docguidelines` (the same source injected into every client's MCP
//! `instructions` and surfaced in `orient` / the `pgmcp://guidelines` resource),
//! so the structured form an agent enumerates here is byte-for-byte the policy it
//! is held to. The `_ctx` argument is unused — there is no project scope and no
//! database access.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use crate::context::SystemContext;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn tool_documentation_guidelines(
    _ctx: &SystemContext,
) -> Result<CallToolResult, McpError> {
    json_result(&crate::docguidelines::guidelines_json())
}
