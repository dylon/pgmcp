//! `csm_show_projection` — show the per-role local machines a pattern projects
//! to (`G ↾ role`), caching them into `csm_projections`. A role that does not
//! project surfaces its `projection_error` rather than being silently dropped.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

use crate::context::SystemContext;
use crate::csm::machine::compile;
use crate::csm::mpst::project::project;
use crate::csm::registry::{ProtocolId, ProtocolParams, global_of};
use crate::csm::role::Role;
use crate::csm::store::{upsert_projection, upsert_protocol};
use crate::mcp::server::CsmShowProjectionParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn tool_csm_show_projection(
    ctx: &SystemContext,
    params: CsmShowProjectionParams,
) -> Result<CallToolResult, McpError> {
    let id = ProtocolId::from_name(&params.protocol)
        .or_else(|| ProtocolId::from_skill_id(&params.protocol))
        .ok_or_else(|| {
            McpError::invalid_params(format!("unknown pattern '{}'", params.protocol), None)
        })?;
    let g = global_of(id, &ProtocolParams::default());
    let want = params.role.as_ref().map(|r| Role::new(r.clone()));

    // Cache the protocol row first so projections can FK to it.
    let pool = ctx.db().pool();
    let mut protocol_id: Option<i64> = None;
    if let Some(pool) = pool
        && let Ok(gjson) = serde_json::to_value(&g)
    {
        let participants: Vec<String> = g.participants().iter().map(|r| r.to_string()).collect();
        protocol_id = upsert_protocol(
            pool,
            id.name(),
            id.pattern_skill_id(),
            &gjson,
            &participants,
            true,
        )
        .await
        .ok();
    }

    let mut projections = Vec::new();
    for role in g.participants() {
        if let Some(w) = &want
            && &role != w
        {
            continue;
        }
        match project(&g, &role) {
            Ok(lt) => {
                let m = compile(&role, &lt);
                let ljson = serde_json::to_value(&lt).ok();
                if let (Some(pool), Some(pid)) = (pool, protocol_id) {
                    let _ = upsert_projection(
                        pool,
                        pid,
                        role.as_str(),
                        ljson.as_ref(),
                        m.n_states as i32,
                        None,
                    )
                    .await;
                }
                projections.push(json!({
                    "role": role.to_string(),
                    "n_states": m.n_states,
                    "local_type": ljson.unwrap_or(Value::Null),
                    "projection_error": Value::Null,
                }));
            }
            Err(e) => {
                if let (Some(pool), Some(pid)) = (pool, protocol_id) {
                    let _ =
                        upsert_projection(pool, pid, role.as_str(), None, 0, Some(&e.message()))
                            .await;
                }
                projections.push(json!({
                    "role": role.to_string(),
                    "projection_error": e.message(),
                }));
            }
        }
    }

    json_result(&json!({
        "protocol": id.name(),
        "requested_role": params.role,
        "projections": projections,
    }))
}
