//! Integration tests for the adaptive per-client tool surface (v37/v38):
//! the `tool_catalog` discovery tool and the machine-learned policy refresh.
//!
//! Requires a real Postgres (migrated) test DB — gated by `require_test_db!`.

use std::collections::HashSet;

use crate::common::{context_with_pool, server_with_pool, text_of};
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

/// Fix 1 (warm-up embed): `warm_mcp_tool_catalog` must seed AND embed every row,
/// so `tool_catalog` semantic ranking works out of the box without the
/// (default-off) embedding-migration cron. With embeddings present, a semantic
/// query then returns scored rows (non-null `score`) — the path that was dead when
/// every embedding stayed NULL.
#[tokio::test(flavor = "multi_thread")]
async fn warm_up_embeds_catalog_and_semantic_search_scores() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let ctx = context_with_pool(pool.clone());

    pgmcp::mcp::tools::tool_meta::warm_mcp_tool_catalog(&ctx)
        .await
        .expect("warm-up must seed + embed");

    let (total, missing) = pgmcp::db::mcp_tool_catalog::counts(&pool)
        .await
        .expect("counts");
    assert!(total > 0, "warm-up must seed the server's own tools");
    assert_eq!(
        missing, 0,
        "warm-up must embed every row, got {missing}/{total} still NULL"
    );

    // With embeddings present, the semantic path returns scored rows.
    let server = server_with_pool(pool);
    let r = server
        .call_tool_cli(
            "tool_catalog",
            json!({ "query": "rank graph nodes by importance", "limit": 5 }),
        )
        .await
        .expect("tool_catalog query");
    let v: serde_json::Value = serde_json::from_str(&text_of(&r)).expect("query JSON");
    let results = v["results"].as_array().expect("results array");
    assert!(
        !results.is_empty(),
        "semantic search must return rows once the catalog is embedded"
    );
    assert!(
        results[0]["score"].is_number(),
        "semantic ranking must yield a non-null score, got {:?}",
        results[0]
    );
}

/// Fix 2 (tokenized keyword fallback): a multi-word natural-language query must
/// match a tool via ANY token as a substring — not only as a verbatim contiguous
/// phrase (the old whole-string `ILIKE` returned nothing for such queries, even
/// when individual words appeared in a description).
#[tokio::test(flavor = "multi_thread")]
async fn keyword_fallback_matches_multiword_query() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    // A probe whose description holds the tokens but NOT the contiguous phrase.
    pgmcp::db::mcp_tool_catalog::upsert_tool(
        &pool,
        "zzz_probe_centrality",
        "graph_core",
        "rank nodes by centrality and pagerank importance",
        "{}",
    )
    .await
    .expect("seed probe row");

    let rows = pgmcp::db::mcp_tool_catalog::keyword_search(
        &pool,
        "centrality pagerank graph importance ranking",
        10,
        None,
    )
    .await
    .expect("keyword_search");

    assert!(
        rows.iter().any(|r| r.name == "zzz_probe_centrality"),
        "tokenized fallback must match a multi-word query; got {:?}",
        rows.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
}

/// Fix 3 (honest, cross-linked guidance): an empty `tool_catalog` result must
/// point the caller at `toolbox_search` — EXTERNAL installed dev tools live in a
/// separate catalog — instead of only blaming a transient embedding backfill.
#[tokio::test(flavor = "multi_thread")]
async fn empty_result_guidance_cross_links_toolbox() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    // A non-existent domain guarantees an empty result regardless of embedding state.
    let r = server
        .call_tool_cli(
            "tool_catalog",
            json!({ "query": "formal verification rocq coq tla", "domain": "zzz_nonexistent_domain" }),
        )
        .await
        .expect("tool_catalog");
    let v: serde_json::Value = serde_json::from_str(&text_of(&r)).expect("JSON");
    assert_eq!(
        v["result_count"].as_i64(),
        Some(0),
        "a non-existent domain must yield an empty result"
    );
    let guidance = v["guidance"].as_str().unwrap_or_default();
    assert!(
        guidance.contains("toolbox_search"),
        "empty-result guidance must cross-link toolbox_search; got: {guidance}"
    );
}
