//! `csm_protocol_to_tla` — render a stored protocol's `GlobalType` as a faithful
//! TLA⁺ module (the global-cursor model) for downstream TLC model-checking.
//!
//! Pure analysis: a `GlobalType` in, a TLA⁺ string out — no checker is spawned, no
//! file written (the caller writes `<module>.tla` and runs TLC). Read-only over
//! pgmcp's own `csm_protocols` table. All encoding lives in
//! [`crate::csm::tla_export::encode_tla`].

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::csm::mpst::global::GlobalType;
use crate::csm::registry::protocol_env;
use crate::csm::tla_export::encode_tla;
use crate::mcp::server::CsmProtocolToTlaParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_csm_protocol_to_tla(
    ctx: &SystemContext,
    params: CsmProtocolToTlaParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    // Resolve the protocol row by public id (preferred) or unique name.
    let row: Option<(i64, String, serde_json::Value)> =
        match (params.protocol_public_id, params.protocol_name.as_deref()) {
            (Some(id), _) => {
                sqlx::query_as("SELECT id, name, global_type FROM csm_protocols WHERE id = $1")
                    .bind(id)
                    .fetch_optional(pool)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("protocol lookup by id failed: {e}"), None)
                    })?
            }
            (None, Some(name)) => {
                sqlx::query_as("SELECT id, name, global_type FROM csm_protocols WHERE name = $1")
                    .bind(name)
                    .fetch_optional(pool)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(
                            format!("protocol lookup by name failed: {e}"),
                            None,
                        )
                    })?
            }
            (None, None) => {
                return Err(McpError::invalid_params(
                    "one of protocol_public_id or protocol_name is required",
                    None,
                ));
            }
        };

    let (id, name, global_json) = row.ok_or_else(|| {
        let which = params
            .protocol_public_id
            .map(|i| format!("id={i}"))
            .or_else(|| params.protocol_name.clone().map(|n| format!("name='{n}'")))
            .unwrap_or_default();
        McpError::invalid_params(
            format!("no protocol found ({which}); list with csm_list_protocols"),
            None,
        )
    })?;

    let g: GlobalType = serde_json::from_value(global_json).map_err(|e| {
        tracing::error!(
            protocol = %name,
            protocol_id = id,
            error = %e,
            "csm_protocol_to_tla: stored global_type failed to decode"
        );
        McpError::internal_error(
            format!("protocol '{name}' has a malformed global_type: {e}"),
            None,
        )
    })?;

    let module = params.module.clone().unwrap_or_else(|| name.clone());
    // Resolve any GlobalCall callees through the registry so a recursive/call-bearing
    // protocol encodes; call-free protocols ignore the env.
    let tla = encode_tla(&g, &protocol_env(), &module)
        .map_err(|e| McpError::internal_error(format!("TLA+ encoding failed: {e}"), None))?;

    json_result(&json!({
        "protocol": name,
        "protocol_public_id": id,
        "module": module,
        "tla": tla,
        "note": "Faithful GlobalType -> TLA+ (global-cursor model): a state cursor `g`, a `fired` \
    label map, and (for box/call protocols) a return-address `stack`. Write `tla` to <module>.tla and \
    model-check with TLC. WellNested/StackBounded are emitted for pushdown protocols; deadlock-freedom is \
    TLC's built-in check; layer data-dependency / liveness assertions over `fired`. For a stack model set \
    MaxStack in the .cfg (>= max nesting / desired recursion bound).",
    }))
}
