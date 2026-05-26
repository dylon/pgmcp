//! `a2a_find_agents_by_specialty` — query registered A2A peers by their
//! specialty tags / recommended role.
//!
//! Inspired by Yang et al. 2026 RecursiveMAS Table 1, where each role
//! (Math Specialist, Code Specialist, etc.) maps to a specific model.
//! For closed-peer A2A we cannot pick models but we can pick peers.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::A2aFindAgentsBySpecialtyParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_a2a_find_agents_by_specialty(
    ctx: &SystemContext,
    params: A2aFindAgentsBySpecialtyParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "a2a_find_agents_by_specialty", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .a2a_specialty_lookups
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let limit = params.limit.unwrap_or(10).clamp(1, 100) as i64;

    let rows: Vec<(String, String, Vec<String>, Option<String>)> =
        sqlx::query_as::<_, (String, String, Vec<String>, Option<String>)>(
            "SELECT name, url, specialty, recommended_role
           FROM a2a_agents
          WHERE specialty && $1::text[]
            AND ($2::text IS NULL OR recommended_role = $2)
          ORDER BY last_seen_at DESC NULLS LAST
          LIMIT $3",
        )
        .bind(&params.specialty)
        .bind(&params.recommended_role)
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Specialty lookup failed: {}", e), None))?;

    let agents: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(name, url, specialty, role)| {
            json!({
                "name": name,
                "url": url,
                "specialty": specialty,
                "recommended_role": role,
            })
        })
        .collect();

    // Shadow-ASR channel (Phase D2b): workspace-wide effect distribution.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT se.effect, COUNT(*)::int8
             FROM symbol_effects se
             GROUP BY se.effect
             ORDER BY se.effect",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        rows.into_iter()
            .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
            .collect()
    })
    .await;

    // Optional typed-capability routing (Pattern H): when the caller supplies
    // required type-tags / effects, additionally rank peers by their
    // structured `capabilities` descriptor (AND-logic on the requirements).
    // Peers without a typed descriptor are filtered out of this list; they
    // remain reachable via the free-text `matches` above.
    let required_type_tags = params.required_type_tags.clone().unwrap_or_default();
    let required_effects = params.required_effects.clone().unwrap_or_default();
    let typed_capability_matches: Vec<serde_json::Value> =
        if required_type_tags.is_empty() && required_effects.is_empty() {
            Vec::new()
        } else {
            use crate::mcp::tools::sema_helpers::a2a_capabilities::{
                AgentMatchFilter, find_agents_by_typed_capability,
            };
            let filter = AgentMatchFilter {
                required_type_tags,
                required_effects,
            };
            match find_agents_by_typed_capability(pool, &filter, limit).await {
                Ok(matches) => matches
                    .into_iter()
                    .map(|m| {
                        json!({
                            "agent_id": m.agent_id,
                            "name": m.name,
                            "specialty": m.specialty,
                            "score": m.score,
                            "capability": {
                                "type_tags": m.capability.type_tags,
                                "effects": m.capability.effects,
                            },
                        })
                    })
                    .collect(),
                Err(e) => {
                    tracing::warn!(error = %e, "typed-capability lookup failed (non-fatal)");
                    Vec::new()
                }
            }
        };

    json_result(&json!({
        "effect_breakdown": effect_breakdown,
        "query": {
            "specialty": params.specialty,
            "recommended_role": params.recommended_role,
            "required_type_tags": params.required_type_tags,
            "required_effects": params.required_effects,
        },
        "matches": agents,
        "typed_capability_matches": typed_capability_matches,
    }))
}
