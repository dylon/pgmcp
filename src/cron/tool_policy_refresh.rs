//! `tool-policy-refresh` cron.
//!
//! Recomputes each client's usage-adaptive default tool set — a recency-decayed
//! usage-frequency score `w = Σ exp(-age/τ)`, thresholded (see
//! [`crate::mcp::tool_policy`] and `docs/design/tool-policy-recency-decay.md`) —
//! from `mcp_tool_calls` telemetry into `client_tool_policy`, then hot-swaps the
//! in-memory [`crate::mcp::tool_policy::ToolPolicySnapshot`] that `list_tools`
//! consults. A cheap SQL aggregation (DELETE + INSERT … SELECT + two SELECTs) —
//! no GPU, no heavy lock — so it runs on its own light schedule rather than
//! behind the heavy-cron gate.

use std::sync::Arc;

use crate::context::SystemContext;
use crate::mcp::tool_policy::{self, ToolPolicyConfig};
use crate::stats::tracker::StatsTracker;

/// Cron entry point: run the refresh and log the outcome.
pub async fn run_or_log(ctx: SystemContext, _stats: Arc<StatsTracker>) {
    match run(&ctx).await {
        Ok(clients) => tracing::info!(clients, "tool-policy-refresh complete"),
        Err(e) => tracing::error!(error = %e, "tool-policy-refresh failed"),
    }
}

async fn run(ctx: &SystemContext) -> Result<usize, sqlx::Error> {
    let Some(pool) = ctx.db().pool() else {
        return Ok(0);
    };
    let cfg = ToolPolicyConfig::default();
    let snapshot = tool_policy::recompute_and_persist(pool, &cfg).await?;
    let clients = snapshot.client_count();
    ctx.set_tool_policy(snapshot);
    Ok(clients)
}
