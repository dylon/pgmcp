//! `csm_protocol_plan` — the protocol interpreter's prescribed orchestrator
//! communication order (ADR-009 Phase 6). For a linearly-drivable pattern this
//! returns the `(peer, request, response)` sequence the `ProtocolDriver` would
//! execute; Deliberation is reported non-drivable (its sender-driven choice is
//! resolved at runtime, so it keeps the hardcoded path).

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::csm::driver::ProtocolDriver;
use crate::csm::machine::Network;
use crate::csm::registry::{ProtocolId, ProtocolParams, global_of, protocol_env};
use crate::csm::role::Role;
use crate::mcp::server::CsmProtocolPlanParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn tool_csm_protocol_plan(
    _ctx: &SystemContext,
    params: CsmProtocolPlanParams,
) -> Result<CallToolResult, McpError> {
    let id = ProtocolId::from_name(&params.pattern)
        .or_else(|| ProtocolId::from_skill_id(&params.pattern))
        .ok_or_else(|| {
            McpError::invalid_params(format!("unknown pattern '{}'", params.pattern), None)
        })?;
    let g = global_of(id, &ProtocolParams::default());
    // Resolve callees through the registry so a call-bearing pattern (RecursiveCf) builds
    // (and is then reported non-drivable); call-free patterns are unaffected by the env.
    let net = Network::build_in(id.name(), &g, &protocol_env()).map_err(|e| {
        McpError::internal_error(format!("projection failed: {}", e.message()), None)
    })?;
    let orchestrator = Role::new("O");
    match ProtocolDriver::plan(&net, &orchestrator) {
        Some(plan) => {
            let steps: Vec<_> = plan
                .iter()
                .map(|s| {
                    json!({
                        "peer": s.peer.to_string(),
                        "request": s.request.name,
                        "response": s.response.name,
                    })
                })
                .collect();
            let n = steps.len();
            json_result(&json!({
                "protocol": id.name(),
                "drivable": true,
                "n_steps": n,
                "plan": steps,
            }))
        }
        None => json_result(&json!({
            "protocol": id.name(),
            "drivable": false,
            "reason": "the orchestrator resolves a sender-driven choice at runtime; \
                not a static linear plan, so it stays on the hardcoded path",
        })),
    }
}
