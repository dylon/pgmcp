//! `csm_protocol_of_pattern` — return one coordination pattern's global type
//! (the adjacent-tagged MPST AST) plus participants and well-formedness.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

use crate::context::SystemContext;
use crate::csm::mpst::wellformed::well_formed;
use crate::csm::registry::{ProtocolId, ProtocolParams, global_of};
use crate::mcp::server::CsmProtocolOfPatternParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn tool_csm_protocol_of_pattern(
    _ctx: &SystemContext,
    params: CsmProtocolOfPatternParams,
) -> Result<CallToolResult, McpError> {
    let id = ProtocolId::from_name(&params.pattern)
        .or_else(|| ProtocolId::from_skill_id(&params.pattern))
        .ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "unknown pattern '{}' (expected one of: sequential, mixture, distillation, \
                     deliberation, recursive — or the a2a_pattern_* skill id)",
                    params.pattern
                ),
                None,
            )
        })?;
    let g = global_of(id, &ProtocolParams::default());
    let wf = well_formed(&g);
    let participants: Vec<String> = g.participants().iter().map(|r| r.to_string()).collect();
    json_result(&json!({
        "name": id.name(),
        "pattern_skill_id": id.pattern_skill_id(),
        "participants": participants,
        "wellformed": wf.is_ok(),
        "wellformed_error": wf.err().map(|e| e.message()),
        "global_type": serde_json::to_value(&g).unwrap_or(Value::Null),
    }))
}
