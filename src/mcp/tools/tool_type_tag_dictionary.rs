//! `tool_type_tag_dictionary` — list the type tag catalog with usage counts
//! and example symbols.
//!
//! Self-documenting view of the type-tag vocabulary. Useful when an
//! agent (or human) wants to know which tags exist and where they appear.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::TypeTagDictionaryParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_type_tag_dictionary(
    ctx: &SystemContext,
    _params: TypeTagDictionaryParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "type_tag_dictionary", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    // Vocabulary entries with descriptions.
    let tag_rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT name, description, language_origin FROM type_tag_catalog ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    // Usage counts per tag (across return_type_tags + symbol_parameters.type_tags).
    let return_counts: Vec<(String, i64)> = sqlx::query_as(
        "SELECT t.tag, COUNT(*)::int8
         FROM file_symbols fs,
              LATERAL unnest(COALESCE(fs.return_type_tags, '{}'::text[])) AS t(tag)
         GROUP BY t.tag",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let param_counts: Vec<(String, i64)> = sqlx::query_as(
        "SELECT t.tag, COUNT(*)::int8
         FROM symbol_parameters p,
              LATERAL unnest(p.type_tags) AS t(tag)
         GROUP BY t.tag",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    use std::collections::HashMap;
    let mut counts: HashMap<String, (i64, i64)> = HashMap::new();
    for (tag, c) in return_counts {
        counts.entry(tag).or_insert((0, 0)).0 = c;
    }
    for (tag, c) in param_counts {
        counts.entry(tag).or_insert((0, 0)).1 = c;
    }

    let tags: Vec<serde_json::Value> = tag_rows
        .into_iter()
        .map(|(name, desc, origin)| {
            let (rc, pc) = counts.get(&name).copied().unwrap_or((0, 0));
            json!({
                "name": name,
                "description": desc,
                "language_origin": origin,
                "return_type_uses": rc,
                "parameter_uses": pc,
                "total_uses": rc + pc,
            })
        })
        .collect();

    // Effect catalog (sibling vocabulary).
    let effect_rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT name, description, language_origin FROM effect_catalog ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let effect_counts: Vec<(String, i64)> =
        sqlx::query_as("SELECT effect, COUNT(*)::int8 FROM symbol_effects GROUP BY effect")
            .fetch_all(pool)
            .await
            .unwrap_or_default();
    let ec: HashMap<String, i64> = effect_counts.into_iter().collect();
    let effects: Vec<serde_json::Value> = effect_rows
        .into_iter()
        .map(|(name, desc, origin)| {
            let c = ec.get(&name).copied().unwrap_or(0);
            json!({
                "name": name,
                "description": desc,
                "language_origin": origin,
                "uses": c,
            })
        })
        .collect();

    json_result(&json!({
        "type_tags": tags,
        "effects": effects,
        "guidance": "Browse the canonical type-tag and effect vocabularies along with their \
                     usage counts. Use `type_shape_search` to query by tag, `effect_propagation` \
                     to trace effect closures."
    }))
}
