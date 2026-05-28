//! `csm_list_protocols` — list the five RecursiveMAS coordination protocols
//! (ADR-009) with participants + well-formedness, caching each into
//! `csm_protocols` when a database pool is available.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::csm::mpst::wellformed::well_formed;
use crate::csm::registry::{ProtocolId, ProtocolParams, global_of};
use crate::csm::store::upsert_protocol;
use crate::mcp::server::CsmListProtocolsParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn tool_csm_list_protocols(
    ctx: &SystemContext,
    _params: CsmListProtocolsParams,
) -> Result<CallToolResult, McpError> {
    let p = ProtocolParams::default();
    let pool = ctx.db().pool();
    let mut out = Vec::with_capacity(ProtocolId::ALL.len());
    for id in ProtocolId::ALL {
        let g = global_of(id, &p);
        let wf = well_formed(&g).is_ok();
        let participants: Vec<String> = g.participants().iter().map(|r| r.to_string()).collect();
        if let Some(pool) = pool
            && let Ok(gjson) = serde_json::to_value(&g)
        {
            let _ = upsert_protocol(
                pool,
                id.name(),
                id.pattern_skill_id(),
                &gjson,
                &participants,
                wf,
            )
            .await;
        }
        out.push(json!({
            "name": id.name(),
            "pattern_skill_id": id.pattern_skill_id(),
            "participants": participants,
            "n_roles": participants.len(),
            "wellformed": wf,
        }));
    }
    let count = out.len();
    json_result(&json!({ "protocols": out, "count": count }))
}
