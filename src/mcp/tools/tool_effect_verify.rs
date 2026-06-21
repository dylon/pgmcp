//! `effect_verify` — effect-policy conformance for a symbol's reachable effects
//! (Task #22 §4-A). In-process; no subprocess, no prattail.
//!
//! The effects reachable from a seed symbol (over the resolved-call subgraph in
//! `sema_helpers::effects`) form a *set*; conformance to a policy is the sound
//! inclusion `reachable ⊆ allowed`. Any reachable effect outside the policy is a
//! violation, reported with the shortest call depth at which it appears — the
//! falsifiable witness.

use std::collections::HashSet;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::EffectVerifyParams;
use crate::mcp::tools::sema_helpers::effects::effects_reachable_from;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_effect_verify(
    ctx: &SystemContext,
    params: EffectVerifyParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let reachable = effects_reachable_from(pool, params.seed_symbol_id, params.max_depth)
        .await
        .map_err(|e| McpError::internal_error(format!("effects_reachable_from: {e}"), None))?;

    let allowed: HashSet<&str> = params.allowed_effects.iter().map(String::as_str).collect();

    let mut violations: Vec<serde_json::Value> = reachable
        .iter()
        .filter(|(name, _)| !allowed.contains(name.as_str()))
        .map(|(name, stats)| json!({ "effect": name, "min_depth": stats.min_depth, "count": stats.count }))
        .collect();
    violations.sort_by_key(|v| v["min_depth"].as_u64().unwrap_or(0));

    let conforms = violations.is_empty();
    let mut reachable_effects: Vec<&String> = reachable.keys().collect();
    reachable_effects.sort();

    json_result(&json!({
        "conforms": conforms,
        "seed_symbol_id": params.seed_symbol_id,
        "violations": violations,
        "reachable_effects": reachable_effects,
        "allowed_effects": params.allowed_effects,
        "method": "effect-set conformance (reachable ⊆ allowed over the resolved-call subgraph)",
    }))
}
