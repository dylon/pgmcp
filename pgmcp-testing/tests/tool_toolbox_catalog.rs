//! Integration coverage for the developer-tool ("toolbox") catalog tools
//! (`toolbox_search` / `toolbox_recommend` / `toolbox_get` / `toolbox_list` /
//! `toolbox_stats` / `toolbox_refresh`), backed by the v32 `tool_cards` table.
//!
//! Satisfies the `query_inventory_vs_coverage` gate: each dispatched tool has a
//! literal `call_tool_cli("…")` here. The harness runs `run_migrations` (so
//! `tool_cards` + its HNSW index exist) with the `DeterministicEmbeddingBackend(1024)`,
//! so the embed legs run end-to-end. The read tools lazily seed the ~111 bundled
//! cards on first call; embeddings stay NULL in tests (the cron does not run), so
//! semantic results are empty but the envelopes are well-formed.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

#[tokio::test]
async fn toolbox_search_executes_against_tool_cards() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    let result = server
        .call_tool_cli(
            "toolbox_search",
            json!({"query": "prove a rewrite system terminates", "limit": 5, "domain": "formal_verification"}),
        )
        .await
        .expect("toolbox_search must not error against the tool_cards HNSW index");
    let v: serde_json::Value =
        serde_json::from_str(&text_of(&result)).expect("toolbox_search body must be JSON");

    assert!(
        v["result_count"].is_number(),
        "result_count must be present"
    );
    assert!(v["results"].is_array(), "results must be an array");
    assert_eq!(
        v["query"].as_str(),
        Some("prove a rewrite system terminates")
    );
}

#[tokio::test]
async fn toolbox_recommend_returns_ranked_envelope() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    let result = server
        .call_tool_cli(
            "toolbox_recommend",
            json!({"task": "find where threads are blocked"}),
        )
        .await
        .expect("toolbox_recommend must not error");
    let v: serde_json::Value =
        serde_json::from_str(&text_of(&result)).expect("toolbox_recommend body must be JSON");

    assert!(
        v["recommended_tools"].is_array(),
        "recommended_tools must be an array"
    );
    // The task keywords ('threads', 'blocked') bias toward developer_tooling.
    assert_eq!(v["domain"].as_str(), Some("developer_tooling"));
}

#[tokio::test]
async fn toolbox_get_seeds_and_returns_known_and_unknown() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    // First call lazily seeds the bundled cards, so a known slug resolves.
    let hit = server
        .call_tool_cli("toolbox_get", json!({"slug_or_id": "z3"}))
        .await
        .expect("toolbox_get must not error");
    let v: serde_json::Value =
        serde_json::from_str(&text_of(&hit)).expect("toolbox_get body must be JSON");
    assert_eq!(v["found"].as_bool(), Some(true), "z3 must be seeded");
    assert_eq!(v["tool"]["slug"].as_str(), Some("z3"));
    assert!(
        v["tool"]["alternatives"].is_array(),
        "alternatives must be an array"
    );

    // An unknown slug returns found:false, not an error.
    let miss = server
        .call_tool_cli(
            "toolbox_get",
            json!({"slug_or_id": "definitely-not-a-tool"}),
        )
        .await
        .expect("toolbox_get on a miss must not error");
    let vm: serde_json::Value =
        serde_json::from_str(&text_of(&miss)).expect("toolbox_get miss body must be JSON");
    assert_eq!(vm["found"].as_bool(), Some(false));
}

#[tokio::test]
async fn toolbox_list_paginates_by_domain() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    let result = server
        .call_tool_cli(
            "toolbox_list",
            json!({"domain": "developer_tooling", "limit": 5, "offset": 0}),
        )
        .await
        .expect("toolbox_list must not error");
    let v: serde_json::Value =
        serde_json::from_str(&text_of(&result)).expect("toolbox_list body must be JSON");

    assert!(v["tools"].is_array(), "tools must be an array");
    assert!(v["count"].is_number(), "count must be present");
    assert_eq!(v["limit"].as_i64(), Some(5));
    let tools = v["tools"].as_array().expect("tools array");
    assert!(tools.len() <= 5, "limit must be respected");
    for t in tools {
        assert_eq!(t["domain"].as_str(), Some("developer_tooling"));
    }
}

#[tokio::test]
async fn toolbox_stats_reports_seeded_counts() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    let result = server
        .call_tool_cli("toolbox_stats", json!({}))
        .await
        .expect("toolbox_stats must not error");
    let v: serde_json::Value =
        serde_json::from_str(&text_of(&result)).expect("toolbox_stats body must be JSON");

    // stats ensure-seeds, so both domains and a non-trivial tool count are present.
    assert!(
        v["stats"]["tools"].as_i64().unwrap_or(0) >= 100,
        "catalog should be seeded"
    );
    assert!(
        v["stats"]["by_domain"].is_array(),
        "by_domain must be an array"
    );
    assert!(
        v["stats"]["by_category"].is_array(),
        "by_category must be an array"
    );
}

#[tokio::test]
async fn toolbox_refresh_dry_run_reports_counts() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    // dry_run avoids any DB write / embedding while still exercising dispatch.
    let result = server
        .call_tool_cli(
            "toolbox_refresh",
            json!({"mode": "seed_only", "dry_run": true}),
        )
        .await
        .expect("toolbox_refresh dry_run must not error");
    let v: serde_json::Value =
        serde_json::from_str(&text_of(&result)).expect("toolbox_refresh body must be JSON");

    assert_eq!(v["dry_run"].as_bool(), Some(true));
    assert!(
        v["tools_seen"].as_i64().unwrap_or(0) >= 100,
        "bundled cards counted"
    );
    assert!(
        v["categories_seen"].is_number(),
        "categories_seen must be present"
    );
}
