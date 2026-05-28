//! `csm_infer_peer_fsm` — Phase-8 passive FSM inference (ADR-009). From a
//! protocol's accumulated run traces (`csm_run_traces.events`), infer the
//! prefix-tree automaton of observed communications and diff it against the
//! declared protocol: novel symbols reveal off-protocol peer behaviour; the
//! conformant fraction measures spec adherence. Passive + frequency-based
//! (active L\* needs a live oracle a nondeterministic LLM peer cannot provide).

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::BTreeSet;

use crate::context::SystemContext;
use crate::csm::inference::infer_prefix_tree;
use crate::csm::registry::{ProtocolId, ProtocolParams, global_of};
use crate::csm::store::{load_protocol_event_traces, protocol_run_stats};
use crate::mcp::server::CsmInferPeerFsmParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_csm_infer_peer_fsm(
    ctx: &SystemContext,
    params: CsmInferPeerFsmParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let id = ProtocolId::from_name(&params.protocol)
        .or_else(|| ProtocolId::from_skill_id(&params.protocol))
        .ok_or_else(|| {
            McpError::invalid_params(format!("unknown pattern '{}'", params.protocol), None)
        })?;
    let min_support = params.min_support.unwrap_or(1).max(1) as usize;

    let traces = load_protocol_event_traces(pool, id.name())
        .await
        .map_err(|e| McpError::internal_error(format!("load traces failed: {e}"), None))?;

    if traces.len() < min_support {
        return json_result(&json!({
            "protocol": id.name(),
            "n_traces": traces.len(),
            "min_support": min_support,
            "status": "insufficient observed runs to infer a model",
        }));
    }

    let fsm = infer_prefix_tree(&traces);

    // Declared alphabet: every (from->to:label) the protocol can emit.
    let g = global_of(id, &ProtocolParams::default());
    let declared: BTreeSet<String> = g
        .communications()
        .into_iter()
        .map(|(f, t, l)| format!("{f}->{t}:{l}"))
        .collect();
    let novel = fsm.novel_symbols(&declared);

    let (total, conformant) = protocol_run_stats(pool, id.name())
        .await
        .unwrap_or((traces.len() as i64, 0));
    let frac = if total > 0 {
        conformant as f64 / total as f64
    } else {
        0.0
    };

    let mut edges = fsm.edges_json();
    edges.truncate(200); // keep the result bounded

    json_result(&json!({
        "protocol": id.name(),
        "n_traces": fsm.n_traces,
        "n_states": fsm.n_states,
        "alphabet_size": fsm.alphabet().len(),
        "declared_alphabet_size": declared.len(),
        "novel_symbols": novel,
        "off_protocol": !novel.is_empty(),
        "total_runs": total,
        "conformant_runs": conformant,
        "conformant_fraction": frac,
        "fsm_edges": edges,
    }))
}
