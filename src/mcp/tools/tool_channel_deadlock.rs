//! `tool_channel_deadlock` — message-passing (channel) deadlock signals over
//! the `sync_ops` message skeleton: blocked receives (no producer), orphan
//! sends (no consumer), and communication cycles (mutually-blocked processes).
//! Soundness is proved in `docs/formal/rocq/ChannelDeadlock.v`.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::concurrency;
use crate::context::SystemContext;
use crate::graph::petri::ChannelFindingKind;
use crate::mcp::server::ChannelDeadlockParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

fn severity_of(k: ChannelFindingKind) -> &'static str {
    match k {
        ChannelFindingKind::ChannelCycle => "critical",
        ChannelFindingKind::BlockedRecv => "high",
        ChannelFindingKind::OrphanSend => "low",
    }
}

pub async fn tool_channel_deadlock(
    ctx: &SystemContext,
    params: ChannelDeadlockParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "channel_deadlock", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50).clamp(1, 500) as usize;

    let (findings, meta) = concurrency::analyze_channels(pool, project_id)
        .await
        .map_err(|e| McpError::internal_error(format!("channel analysis failed: {e}"), None))?;

    let mut out = Vec::new();
    for f in &findings {
        let procs: Vec<_> = f
            .processes
            .iter()
            .map(|id| {
                let m = meta.get(id);
                json!({
                    "symbol_id": id,
                    "name": m.map(|m| m.name.clone()),
                    "file": m.map(|m| m.relative_path.clone()),
                })
            })
            .collect();
        let waits: Vec<_> = f
            .waits
            .iter()
            .map(|(id, ch)| json!({"symbol_id": id, "waits_on": ch}))
            .collect();
        out.push(json!({
            "finding_kind": f.kind.as_str(),
            "channel": f.channel,
            "processes": procs,
            "waits": waits,
            "severity": severity_of(f.kind),
            "detail": f.detail,
        }));
        if out.len() >= limit {
            break;
        }
    }

    json_result(&json!({
        "findings": out,
        "returned": out.len(),
        "total": findings.len(),
        "guidance": "Channel-deadlock signals (Petri-net structural analysis): blocked_recv (a \
            linear receive whose channel has no producer anywhere → blocks forever), orphan_send \
            (a channel sent-to but never received), channel_cycle (processes each initially \
            blocked on a receive only the next blocked process produces). Soundness proved in \
            docs/formal/rocq/ChannelDeadlock.v. Rholang persistent receives (`<=`) stay armed and \
            are excluded from blocking analysis."
    }))
}
