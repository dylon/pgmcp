//! `csm_protocol_string_diagram` (ADR-028, CT-3) — the **monoidal
//! string-diagram decomposition** of a real protocol's `GlobalType`, loaded from
//! the `csm_protocols` table by public id or name.
//!
//! The actionable, falsifiable core is COMPUTED, not drawn:
//! - **Sequential composition** (`;`): the consecutive-interaction spine, whose
//!   `sequential_depth` is the number of interaction steps on the longest single
//!   trace.
//! - **Tensor** (`⊗`): the partition of roles into independent parallel
//!   sub-protocols (union-find over every role pair that co-occurs in an
//!   `Interaction` or `Choice`). Two roles in different factors **provably never
//!   communicate** in this protocol — a schedule-relevant, checkable claim.
//!
//! All structure comes from [`crate::csm::string_diagram::decompose`]; the
//! unicode diagram is a secondary rendering. Read-only over pgmcp's own tables.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::csm::mpst::global::GlobalType;
use crate::csm::string_diagram::{decompose, render};
use crate::mcp::server::CsmProtocolStringDiagramParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_csm_protocol_string_diagram(
    ctx: &SystemContext,
    params: CsmProtocolStringDiagramParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    // Resolve the protocol row by public id (preferred) or unique name. Exactly
    // one selector is required.
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

    // Decode the stored adjacent-tagged GlobalType. A decode failure is a real
    // data-integrity problem (a malformed registry row), surfaced loudly.
    let g: GlobalType = serde_json::from_value(global_json).map_err(|e| {
        tracing::error!(
            protocol = %name,
            protocol_id = id,
            error = %e,
            "csm_protocol_string_diagram: stored global_type failed to decode"
        );
        McpError::internal_error(
            format!("protocol '{name}' has a malformed global_type: {e}"),
            None,
        )
    })?;

    let d = decompose(&g);
    let diagram = render(&name, &d);

    json_result(&json!({
        "protocol": name,
        "protocol_public_id": id,
        "roles": d.roles,
        "boxes": d.boxes,
        "tensor_factors": d.tensor_factors,
        "n_tensor_factors": d.n_tensor_factors,
        "sequential_depth": d.sequential_depth,
        "recursion": {
            "has_rec": d.recursion.has_rec,
            "vars": d.recursion.vars,
        },
        "diagram": diagram,
        "note": "Monoidal decomposition of the protocol read as a morphism over role wires (CT-3). \
    sequential_depth = interaction steps on the longest single trace (; spine). tensor_factors = \
    independent parallel sub-protocols (⊗): roles in DIFFERENT factors provably never communicate \
    in this protocol, so they may be scheduled on independent executors. Rec back-edges loop the spine.",
    }))
}
