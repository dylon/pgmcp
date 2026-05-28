//! Layer A of the integration-test plan: SQL-execution smoke tests for
//! MCP tool handlers that read from real Postgres tables.
//!
//! Each test seeds the synthetic corpus (which populates `code_topics`,
//! `chunk_topic_assignments`, `file_metrics`, etc.), calls a tool via
//! `McpServer::call_tool_cli`, and asserts the call returns `Ok`. The
//! point is not to check algorithmic correctness — the `oracle_*.rs`
//! files cover that — but to catch the orient-class bug: a SQL string
//! that drifts from the schema and only fails when the derived table
//! is populated.
//!
//! See `/home/dylon/.claude/plans/identify-the-root-cause-functional-wren.md`
//! for the full plan (Layers A–D).

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::SyntheticCorpus;
use pgmcp_testing::require_test_db;
use serde_json::json;

// =============================================================================
// orient — the tool whose regression motivated this entire test layer.
//
// Before the fix (commit 802ca00), this query failed with
// `column "topic_id" does not exist` and later
// `column "member_count" does not exist`, plus a scope-literal mismatch
// (`'*'` vs the actual `'global'`).  We assert the response is a
// well-formed JSON envelope and that `top_topics` is reachable.
// =============================================================================
#[tokio::test]
async fn tool_orient_against_populated_corpus_resolves_topics() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli("orient", json!({"project": "proj-auth"}))
        .await
        .expect("orient must not error against populated code_topics");

    let v: serde_json::Value =
        serde_json::from_str(&text_of(&result)).expect("orient body must be JSON");

    // Envelope shape — required keys present.
    assert_eq!(
        v["found"].as_bool(),
        Some(true),
        "orient must find the project"
    );
    assert!(v["top_topics"].is_array(), "top_topics must be an array");
    assert!(v["languages"].is_array(), "languages must be an array");
    assert!(
        v["tree_depth_2"].is_array(),
        "tree_depth_2 must be an array"
    );
    assert!(v["health"].is_object(), "health envelope must be present");

    // The synthetic corpus plants 3 global topics (auth, database, logging)
    // with `scope = 'global'`. orient's WHERE clause matches scope = 'global'
    // for the fallback path, so we must see those topics.
    let topics = v["top_topics"].as_array().unwrap();
    assert!(
        !topics.is_empty(),
        "top_topics must be non-empty against the seeded corpus; \
         got empty list which suggests the scope literal regressed"
    );

    // Each topic must have the keys the JSON contract promises.
    for (idx, t) in topics.iter().enumerate() {
        assert!(t["topic_id"].is_number(), "topic {} missing topic_id", idx);
        assert!(t["scope"].is_string(), "topic {} missing scope", idx);
        assert!(
            t["member_count"].is_number(),
            "topic {} missing member_count",
            idx
        );
    }
}

// =============================================================================
// Layer A — generic smoke tests for previously-uncovered tools.
//
// Each test calls a tool with minimal valid arguments and asserts it
// doesn't return a SQL error. Algorithmic correctness is out of scope
// here; the oracle_*.rs tests cover that for tools that have them.
// =============================================================================

#[tokio::test]
async fn tool_topic_hierarchy_fcm_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli("topic_hierarchy_fcm", json!({}))
        .await
        .expect("topic_hierarchy_fcm must not error");
}

#[tokio::test]
async fn tool_naming_consistency_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli(
            "naming_consistency",
            json!({"project": "proj-auth", "language": "rust"}),
        )
        .await
        .expect("naming_consistency must not error");
}

#[tokio::test]
async fn tool_tech_debt_burn_down_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli("tech_debt_burn_down", json!({"project": "proj-auth"}))
        .await
        .expect("tech_debt_burn_down must not error");
}

#[tokio::test]
async fn tool_extraction_candidates_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli("extraction_candidates", json!({}))
        .await
        .expect("extraction_candidates must not error");
}

#[tokio::test]
async fn tool_boilerplate_clusters_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli("boilerplate_clusters", json!({}))
        .await
        .expect("boilerplate_clusters must not error");
}

#[tokio::test]
async fn tool_chunk_clusters_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli("chunk_clusters", json!({}))
        .await
        .expect("chunk_clusters must not error");
}

#[tokio::test]
async fn tool_dependency_health_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli("dependency_health", json!({"project": "proj-auth"}))
        .await
        .expect("dependency_health must not error");
}

#[tokio::test]
async fn tool_merge_conflict_risk_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli("merge_conflict_risk", json!({"project": "proj-auth"}))
        .await
        .expect("merge_conflict_risk must not error");
}

#[tokio::test]
async fn tool_hot_path_audit_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli("hot_path_audit", json!({"project": "proj-auth"}))
        .await
        .expect("hot_path_audit must not error");
}

#[tokio::test]
async fn tool_bus_factor_map_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli("bus_factor_map", json!({"project": "proj-auth"}))
        .await
        .expect("bus_factor_map must not error");
}

#[tokio::test]
async fn tool_stale_zombie_detector_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli("stale_zombie_detector", json!({"project": "proj-auth"}))
        .await
        .expect("stale_zombie_detector must not error");
}

#[tokio::test]
async fn tool_module_growth_trajectory_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli("module_growth_trajectory", json!({"project": "proj-auth"}))
        .await
        .expect("module_growth_trajectory must not error");
}

#[tokio::test]
async fn tool_reviewer_recommender_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli(
            "reviewer_recommender",
            json!({"project": "proj-auth", "file": "src/lib.rs"}),
        )
        .await
        .expect("reviewer_recommender must not error");
}

#[tokio::test]
async fn tool_recommend_layering_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli("recommend_layering", json!({"project": "proj-auth"}))
        .await
        .expect("recommend_layering must not error");
}

#[tokio::test]
async fn tool_recommend_module_split_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli("recommend_module_split", json!({"project": "proj-auth"}))
        .await
        .expect("recommend_module_split must not error");
}

#[tokio::test]
async fn tool_adoption_lag_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli("adoption_lag", json!({"new_file": "proj-auth:src/lib.rs"}))
        .await
        .expect("adoption_lag must not error");
}

#[tokio::test]
async fn tool_fix_circular_dependency_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli("fix_circular_dependency", json!({"project": "proj-auth"}))
        .await
        .expect("fix_circular_dependency must not error");
}

#[tokio::test]
async fn tool_get_software_pattern_against_seeded_catalog() {
    // The pattern catalog is seeded at migration time (src/db/patterns.rs).
    // We pick a slug that the seed catalog is known to contain. If the
    // slug doesn't exist the tool returns Ok with an error envelope; we
    // only care that the SQL executes.
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli(
            "get_software_pattern",
            json!({"slug_or_id": "gof_singleton"}),
        )
        .await
        .expect("get_software_pattern must not error");
}

#[tokio::test]
async fn tool_internal_dry_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli("internal_dry", json!({"file": "proj-auth:src/lib.rs"}))
        .await
        .expect("internal_dry must not error");
}

#[tokio::test]
async fn tool_mcp_tool_telemetry_against_empty_table() {
    // `mcp_tool_calls` is empty in a fresh test DB. The SQL must still
    // parse and the aggregate must return zero rows rather than erroring.
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli("mcp_tool_telemetry", json!({}))
        .await
        .expect("mcp_tool_telemetry must not error");
}

#[tokio::test]
async fn tool_pattern_abstraction_candidates_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli("pattern_abstraction_candidates", json!({}))
        .await
        .expect("pattern_abstraction_candidates must not error");
}

#[tokio::test]
async fn tool_pattern_search_against_seeded_catalog() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli(
            "pattern_search",
            json!({"snippet": "fn validate_password(p: &str) -> bool { !p.is_empty() }"}),
        )
        .await
        .expect("pattern_search must not error");
}

#[tokio::test]
async fn tool_pr_scope_recommender_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli(
            "pr_scope_recommender",
            json!({"project": "proj-auth", "file": "src/lib.rs"}),
        )
        .await
        .expect("pr_scope_recommender must not error");
}

#[tokio::test]
async fn tool_refresh_pattern_catalog_dry_run() {
    // dry_run = true so the tool doesn't actually rewrite the catalog
    // (which we want left as-seeded). We're only testing the SQL paths.
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli(
            "refresh_pattern_catalog",
            json!({"mode": "seed_only", "dry_run": true}),
        )
        .await
        .expect("refresh_pattern_catalog must not error");
}

#[tokio::test]
async fn tool_reindex_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli("reindex", json!({}))
        .await
        .expect("reindex must not error");
}

#[tokio::test]
async fn tool_shotgun_surgery_fix_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli("shotgun_surgery_fix", json!({"project": "proj-auth"}))
        .await
        .expect("shotgun_surgery_fix must not error");
}

#[tokio::test]
async fn tool_upsert_pattern_source_against_seeded_catalog() {
    // Insert a fresh source row for a seeded pattern. Idempotent on
    // conflict; we just need the SQL to execute. Pattern slug must
    // exist in the seeded catalog.
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let _ = server
        .call_tool_cli(
            "upsert_pattern_source",
            json!({
                "pattern_slug": "gof_singleton",
                "source_family": "test",
                "source_type": "manual",
                "title": "smoke-test source"
            }),
        )
        .await
        .expect("upsert_pattern_source must not error");
}

// =============================================================================
// adoption_report — the independent telemetry collector (src/adoption) over
// mcp_tool_calls (+ nudge_emissions / csm_run_traces). Smoke-test that the
// per-family aggregation, the nudge→adoption conversion CASE, and the CSM
// conformance query all execute against a real schema (data may be empty).
// =============================================================================
#[tokio::test]
async fn tool_adoption_report_executes_against_real_schema() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli("adoption_report", json!({ "format": "json" }))
        .await
        .expect("adoption_report must not error against the real schema");

    let v: serde_json::Value =
        serde_json::from_str(&text_of(&result)).expect("adoption_report body must be JSON");

    // Envelope shape — required keys present (the data itself may be empty on a
    // fresh DB; we are checking the SQL executes and the report assembles).
    assert!(v["window_minutes"].is_number());
    assert!(v["allowlist"].is_array(), "allowlist must be an array");
    assert!(v["overall"].is_array(), "overall must be an array");
    assert!(v["clients"].is_array(), "clients must be an array");
    assert!(v["conversion"].is_array(), "conversion must be an array");
    assert!(
        v["csm_conformance"].is_object(),
        "csm_conformance must be an object"
    );
}
