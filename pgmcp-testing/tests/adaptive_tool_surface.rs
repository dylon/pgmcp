//! Integration tests for the adaptive per-client tool surface (v37/v38):
//! the `tool_catalog` discovery tool and the machine-learned policy refresh.
//!
//! Requires a real Postgres (migrated) test DB — gated by `require_test_db!`.

mod common;

use std::collections::HashSet;

use common::{server_with_pool, text_of};
use pgmcp::mcp::client_profile::{ClientProfile, ToolSurface};
use pgmcp::mcp::server::McpServer;
use pgmcp::mcp::tool_policy::{ToolPolicyConfig, recompute_and_persist};
use pgmcp_testing::require_test_db;
use serde_json::json;

/// `tool_catalog` seeds the server's own tools and ranks them; keyword fallback
/// (no GPU needed) must surface a known tool by name. Also exercises the
/// dispatch-coverage gate (`call_tool_cli("tool_catalog", …)`).
#[tokio::test(flavor = "multi_thread")]
async fn tool_catalog_lists_and_searches_own_tools() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    let r = server
        .call_tool_cli("tool_catalog", json!({ "limit": 5 }))
        .await
        .expect("tool_catalog");
    assert!(r.is_error != Some(true));
    let v: serde_json::Value = serde_json::from_str(&text_of(&r)).expect("tool_catalog JSON");
    assert!(
        v["results"].as_array().is_some_and(|a| !a.is_empty()),
        "catalog should list the server's own tools"
    );

    // Keyword query (semantic embedding may be unavailable in CI → ILIKE fallback)
    // must still find a tool by name.
    let r2 = server
        .call_tool_cli(
            "tool_catalog",
            json!({ "query": "semantic_search", "limit": 8 }),
        )
        .await
        .expect("tool_catalog query");
    let v2: serde_json::Value = serde_json::from_str(&text_of(&r2)).expect("query JSON");
    let empty = Vec::new();
    let names: Vec<String> = v2["results"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|row| row["name"].as_str().map(String::from))
        .collect();
    assert!(
        names.iter().any(|n| n == "semantic_search"),
        "tool_catalog keyword search should surface semantic_search: {names:?}"
    );
}

/// The learner promotes a frequently + recently used tool into a client's learned
/// defaults (weight ≥ threshold) and decays an old one below it — and the derived
/// snapshot exposes the promoted tool in that client's Learned surface even though
/// it is not in the mandatory core.
#[tokio::test(flavor = "multi_thread")]
async fn learner_promotes_used_tools_and_decays_old_ones() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let client = "codex-policy-test";

    // Start from a clean slate for this synthetic client.
    sqlx::query("DELETE FROM mcp_tool_calls WHERE client_name = $1")
        .bind(client)
        .execute(&pool)
        .await
        .expect("clean mcp_tool_calls");

    // Frequent, recent usage of a real long-tail tool.
    for _ in 0..5 {
        sqlx::query(
            "INSERT INTO mcp_tool_calls (tool, client_name, duration_ms, outcome) \
             VALUES ('central_functions', $1, 1, 'ok')",
        )
        .bind(client)
        .execute(&pool)
        .await
        .expect("insert recent usage");
    }
    // A single, ancient use that should decay below threshold.
    sqlx::query(
        "INSERT INTO mcp_tool_calls (tool, client_name, ts, duration_ms, outcome) \
         VALUES ('ancient_tool', $1, now() - interval '60 days', 1, 'ok')",
    )
    .bind(client)
    .execute(&pool)
    .await
    .expect("insert ancient usage");

    let cfg = ToolPolicyConfig::default();
    let snapshot = recompute_and_persist(&pool, &cfg)
        .await
        .expect("recompute_and_persist");

    // Materialized weights: frequent ≥ threshold, ancient decayed below it.
    let recent_weight: f64 = sqlx::query_scalar(
        "SELECT weight FROM client_tool_policy WHERE client_name = $1 AND tool_name = 'central_functions'",
    )
    .bind(client)
    .fetch_one(&pool)
    .await
    .expect("recent weight");
    assert!(
        recent_weight >= cfg.weight_threshold,
        "frequently-used tool weight {recent_weight} must cross the threshold {}",
        cfg.weight_threshold
    );
    let ancient_weight: Option<f64> = sqlx::query_scalar(
        "SELECT weight FROM client_tool_policy WHERE client_name = $1 AND tool_name = 'ancient_tool'",
    )
    .bind(client)
    .fetch_optional(&pool)
    .await
    .expect("ancient weight query");
    assert!(
        ancient_weight.is_none_or(|w| w < cfg.weight_threshold),
        "a 60-day-old single use should decay below threshold, got {ancient_weight:?}"
    );

    // The snapshot exposes the promoted tool in this client's Learned surface,
    // even though `central_functions` is NOT in the mandatory core.
    let profile = ClientProfile {
        tool_surface: ToolSurface::Learned,
        ..ClientProfile::default()
    };
    let mut tools = McpServer::static_tool_catalog();
    snapshot.retain_exposed(&mut tools, &profile, client, &HashSet::new());
    assert!(
        tools.iter().any(|t| t.name.as_ref() == "central_functions"),
        "the learned tool must appear in the client's Learned surface"
    );
    assert!(
        tools.iter().all(|t| t.name.as_ref() != "ancient_tool"),
        "a decayed/absent tool must not appear"
    );
}
