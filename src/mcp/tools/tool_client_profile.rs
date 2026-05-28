//! Memory-server Phase 10: `pgmcp_client_profile` introspection tool.
//!
//! Lets the agent ask "what profile am I being served under?" or
//! "show me every profile pgmcp knows about". Pure-read tool; no
//! side effects.
//!
//! Phase D2b shadow-ASR contract: this tool serializes the client profile
//! using `format.serialize_value` (TOML / YAML / JSON depending on the
//! profile's `output_format`). The output shape is wholly defined by that
//! enum and is not a generic JSON envelope, so the workspace-wide
//! `effect_breakdown` channel is intentionally NOT mixed in here.
//! Clients that want effect-distribution data should call
//! `tool_index_stats` (Pattern F enrichment) or any of the project-scoped
//! analysis tools, which all surface the effect_breakdown channel.

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::PgmcpClientProfileParams;

/// Resolve the on-disk profiles path. Defaults to `assets/client_profiles.toml`
/// relative to the binary's working directory. Overridable via
/// `PGMCP_CLIENT_PROFILES_PATH` for tests / production deployments
/// that ship the asset elsewhere.
pub(crate) fn profiles_path() -> PathBuf {
    if let Ok(p) = std::env::var("PGMCP_CLIENT_PROFILES_PATH") {
        return PathBuf::from(p);
    }
    PathBuf::from("assets/client_profiles.toml")
}

pub async fn tool_pgmcp_client_profile(
    ctx: &SystemContext,
    params: PgmcpClientProfileParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "pgmcp_client_profile", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    // Use the registry cached on SystemContext (loaded once) rather than
    // re-reading the TOML on every call.
    let registry = ctx.client_profiles();

    if params.list_all.unwrap_or(false) {
        let profiles: Vec<&_> = registry.all();
        let text = serde_json::to_string_pretty(&json!({
            "count": profiles.len(),
            "profiles": profiles,
        }))
        .map_err(|e| McpError::internal_error(format!("serialize: {}", e), None))?;
        return Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            text,
        )]));
    }

    let client_name = params.client_name.unwrap_or_else(|| "generic".into());
    let profile = registry.for_client(&client_name);
    let format = profile.output_format;
    let payload = serde_json::to_value(profile)
        .map_err(|e| McpError::internal_error(format!("serialize: {}", e), None))?;
    let text = format.serialize_value(&payload);
    Ok(CallToolResult::success(vec![rmcp::model::Content::text(
        text,
    )]))
}
