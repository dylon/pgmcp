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

#[tokio::test]
async fn tool_orient_project_snapshot_does_not_leak_other_projects() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let h = SyntheticCorpus::seed_with_assignments(&pool).await;

    let auth_file_id: i64 = sqlx::query_scalar(
        "SELECT f.id
         FROM indexed_files f
         JOIN projects p ON p.id = f.project_id
         WHERE p.name = 'proj-auth' AND f.relative_path = 'auth/file_0.rs'",
    )
    .fetch_one(&pool)
    .await
    .expect("auth fixture file");
    let database_file_id: i64 = sqlx::query_scalar(
        "SELECT f.id
         FROM indexed_files f
         JOIN projects p ON p.id = f.project_id
         WHERE p.name = 'proj-database' AND f.relative_path = 'database/file_0.rs'",
    )
    .fetch_one(&pool)
    .await
    .expect("database fixture file");
    sqlx::query(
        "INSERT INTO file_metrics
            (file_id, project_id, pagerank, in_degree, out_degree)
         VALUES
            ($1, $2, 0.90, 1, 0),
            ($3, $4, 0.99, 2, 1)",
    )
    .bind(auth_file_id)
    .bind(h.auth_project_id)
    .bind(database_file_id)
    .bind(h.database_project_id)
    .execute(&pool)
    .await
    .expect("seed file metrics");

    let server = server_with_pool(pool);
    let result = server
        .call_tool_cli("orient", json!({"project": "proj-auth"}))
        .await
        .expect("orient must not error");
    let v: serde_json::Value =
        serde_json::from_str(&text_of(&result)).expect("orient body must be JSON");

    assert_eq!(v["project_name"].as_str(), Some("proj-auth"));
    assert_eq!(v["project_root"].as_str(), Some("/ws/auth/proj-auth"));

    for key in ["tree_depth_2", "recently_changed", "key_entry_points"] {
        let rows = v[key].as_array().expect("orient path array");
        assert!(!rows.is_empty(), "{key} must have fixture rows");
        for row in rows {
            let path = row
                .as_str()
                .or_else(|| row["path"].as_str())
                .expect("orient row path");
            assert!(
                !path.starts_with("database/") && !path.starts_with("logging/"),
                "{key} leaked a non-auth project path: {path}"
            );
        }
    }

    let entry_paths: Vec<&str> = v["key_entry_points"]
        .as_array()
        .expect("key_entry_points array")
        .iter()
        .filter_map(|row| row["path"].as_str())
        .collect();
    assert_eq!(entry_paths, vec!["auth/file_0.rs"]);
}

#[tokio::test]
async fn tool_grep_project_filter_does_not_leak_other_projects() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let unscoped = server
        .call_tool_cli("grep", json!({"pattern": "password", "limit": 50}))
        .await
        .expect("unscoped grep must not error");
    let unscoped: serde_json::Value =
        serde_json::from_str(&text_of(&unscoped)).expect("grep body must be JSON");
    let unscoped_hits = unscoped["hits"].as_array().expect("hits array");
    assert!(
        unscoped_hits
            .iter()
            .any(|h| h["project_name"].as_str() == Some("proj-database")),
        "fixture must include a cross-project password hit for this regression"
    );

    let scoped = server
        .call_tool_cli(
            "grep",
            json!({"pattern": "password", "project": "proj-auth", "limit": 50}),
        )
        .await
        .expect("scoped grep must not error");
    let scoped: serde_json::Value =
        serde_json::from_str(&text_of(&scoped)).expect("grep body must be JSON");
    let scoped_hits = scoped["hits"].as_array().expect("hits array");
    assert!(!scoped_hits.is_empty(), "proj-auth should have grep hits");
    assert!(
        scoped_hits
            .iter()
            .all(|h| h["project_name"].as_str() == Some("proj-auth")),
        "project-scoped grep leaked another project: {scoped_hits:?}"
    );
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
async fn tool_mcp_tool_telemetry_filters_project_across_aggregations() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool.clone());

    sqlx::query(
        "INSERT INTO mcp_tool_calls
            (tool, client_name, project, duration_ms, outcome)
         VALUES
            ('semantic_search', 'cli', 'pgmcp', 10, 'ok'),
            ('grep', 'cli', 'pgmcp', 20, 'error'),
            ('semantic_search', 'cli', 'other', 30, 'ok'),
            ('grep', 'claude-code', 'other', 40, 'ok'),
            ('orient', 'cli', '', 50, 'ok')",
    )
    .execute(&pool)
    .await
    .expect("seed telemetry rows");

    let top_tools = server
        .call_tool_cli(
            "mcp_tool_telemetry",
            json!({"aggregation": " top_tools ", "project": " pgmcp "}),
        )
        .await
        .expect("top_tools telemetry must not error");
    let top_tools: serde_json::Value =
        serde_json::from_str(&text_of(&top_tools)).expect("top_tools body must be JSON");
    assert_eq!(top_tools["aggregation"].as_str(), Some("top_tools"));
    assert_eq!(top_tools["filters"]["project"].as_str(), Some("pgmcp"));
    let rows = top_tools["data"]["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 2, "top_tools must only include pgmcp rows");
    assert!(
        rows.iter()
            .any(|r| r["tool"].as_str() == Some("semantic_search"))
    );
    assert!(rows.iter().any(|r| r["tool"].as_str() == Some("grep")));
    assert!(!rows.iter().any(|r| r["tool"].as_str() == Some("orient")));

    let top_callers = server
        .call_tool_cli(
            "mcp_tool_telemetry",
            json!({"aggregation": "top_callers", "tool": " grep ", "project": " pgmcp "}),
        )
        .await
        .expect("top_callers telemetry must not error");
    let top_callers: serde_json::Value =
        serde_json::from_str(&text_of(&top_callers)).expect("top_callers body must be JSON");
    assert_eq!(top_callers["filters"]["tool"].as_str(), Some("grep"));
    assert_eq!(top_callers["filters"]["project"].as_str(), Some("pgmcp"));
    let rows = top_callers["data"]["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["client_name"].as_str(), Some("cli"));
    assert_eq!(rows[0]["calls"].as_i64(), Some(1));

    let top_projects = server
        .call_tool_cli(
            "mcp_tool_telemetry",
            json!({
                "aggregation": "top_projects",
                "tool": "semantic_search",
                "client_name": "cli",
                "project": "pgmcp"
            }),
        )
        .await
        .expect("top_projects telemetry must not error");
    let top_projects: serde_json::Value =
        serde_json::from_str(&text_of(&top_projects)).expect("top_projects body must be JSON");
    let rows = top_projects["data"]["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["project"].as_str(), Some("pgmcp"));
    assert_eq!(rows[0]["calls"].as_i64(), Some(1));

    let error_rate = server
        .call_tool_cli(
            "mcp_tool_telemetry",
            json!({"aggregation": "error_rate", "tool": "grep", "project": "pgmcp"}),
        )
        .await
        .expect("error_rate telemetry must not error");
    let error_rate: serde_json::Value =
        serde_json::from_str(&text_of(&error_rate)).expect("error_rate body must be JSON");
    let rows = error_rate["data"]["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["tool"].as_str(), Some("grep"));
    assert_eq!(rows[0]["calls"].as_i64(), Some(1));
    assert_eq!(rows[0]["errors"].as_i64(), Some(1));

    let summary = server
        .call_tool_cli(
            "mcp_tool_telemetry",
            json!({"aggregation": "summary", "project": "pgmcp"}),
        )
        .await
        .expect("summary telemetry must not error");
    let summary: serde_json::Value =
        serde_json::from_str(&text_of(&summary)).expect("summary body must be JSON");
    let rows = summary["data"]["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 2, "summary must only include pgmcp rows");
    assert!(rows.iter().all(|r| r["project"].as_str() == Some("pgmcp")));
    assert!(!rows.iter().any(|r| r["tool"].as_str() == Some("orient")));

    let histogram = server
        .call_tool_cli(
            "mcp_tool_telemetry",
            json!({"aggregation": "histogram", "tool": "semantic_search", "project": "pgmcp"}),
        )
        .await
        .expect("histogram telemetry must not error");
    let histogram: serde_json::Value =
        serde_json::from_str(&text_of(&histogram)).expect("histogram body must be JSON");
    let rows = histogram["data"]["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["tool"].as_str(), Some("semantic_search"));
    assert_eq!(rows[0]["bucket"].as_i64(), Some(2));
    assert_eq!(rows[0]["count"].as_i64(), Some(1));

    let raw = server
        .call_tool_cli(
            "mcp_tool_telemetry",
            json!({"aggregation": "raw", "project": "pgmcp", "limit": 10}),
        )
        .await
        .expect("raw telemetry must not error");
    let raw: serde_json::Value =
        serde_json::from_str(&text_of(&raw)).expect("raw body must be JSON");
    let rows = raw["data"]["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 2, "raw must only include pgmcp rows");
    assert!(rows.iter().all(|r| r["project"].as_str() == Some("pgmcp")));
    assert!(
        rows.iter()
            .any(|r| r["tool"].as_str() == Some("semantic_search"))
    );
    assert!(rows.iter().any(|r| r["tool"].as_str() == Some("grep")));

    let raw_clamped = server
        .call_tool_cli(
            "mcp_tool_telemetry",
            json!({"aggregation": "raw", "project": " pgmcp ", "limit": -10}),
        )
        .await
        .expect("raw telemetry with low limit must not error");
    let raw_clamped: serde_json::Value =
        serde_json::from_str(&text_of(&raw_clamped)).expect("raw clamped body must be JSON");
    let rows = raw_clamped["data"]["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 1, "negative raw limit must clamp to one row");
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
    sqlx::query(
        "INSERT INTO mcp_tool_calls
            (ts, tool, client_name, mcp_session_id, duration_ms, outcome)
         VALUES
            (now(), 'a2a_send_task', 'claude-code', 's1', 10, 'ok'),
            (now(), 'a2a_pattern_recursive', 'claude-code', 's1', 11, 'ok'),
            (now(), 'memory_unified_search', 'claude-code', 's2', 12, 'ok'),
            (now(), 'work_item_create', 'claude-code', 's3', 13, 'ok'),
            (now(), 'semantic_search', 'claude-code', 's4', 14, 'ok'),
            (now(), 'a2a_send_task', 'cli', 'cli1', 15, 'ok'),
            (now() - interval '2 hours', 'memory_unified_search', 'claude-code', 'old', 16, 'ok')",
    )
    .execute(&pool)
    .await
    .expect("seed adoption telemetry");
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "adoption_report",
            json!({ "format": " json ", "since_minutes": 60 }),
        )
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
    assert_eq!(v["window_minutes"].as_i64(), Some(60));
    assert_eq!(v["overall_total_calls"].as_i64(), Some(5));
    let clients = v["clients"].as_array().expect("clients");
    assert_eq!(clients.len(), 1, "only real clients should be counted");
    assert_eq!(clients[0]["client_name"].as_str(), Some("claude-code"));
    assert_eq!(clients[0]["total_calls"].as_i64(), Some(5));
    assert_eq!(clients[0]["total_sessions"].as_i64(), Some(4));

    let family_calls = |family: &str| -> i64 {
        v["overall"]
            .as_array()
            .expect("overall")
            .iter()
            .find(|row| row["family"].as_str() == Some(family))
            .and_then(|row| row["calls"].as_i64())
            .unwrap_or(-1)
    };
    assert_eq!(family_calls("A2A collaboration"), 2);
    assert_eq!(family_calls("RLM (recursive)"), 1);
    assert_eq!(family_calls("Memory server"), 1);
    assert_eq!(family_calls("Work-item tracker"), 1);
    assert_eq!(family_calls("CSM coordination-conformance"), 0);
}

// =============================================================================
// CT-4 — fca_concept_lattice (ADR-028 / Crucible ADR-010). The Galois
// derivation operators + NextClosure concept enumeration over a real
// (objects × attributes) context. With no effect incidence on the synthetic
// corpus the lattice is the trivial top/bottom, but the SQL + closure must
// execute against the real schema.
// =============================================================================
#[tokio::test]
async fn tool_fca_concept_lattice_against_populated_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "fca_concept_lattice",
            json!({"project": "proj-auth", "object_kind": "symbol", "attribute_kind": "effect"}),
        )
        .await
        .expect("fca_concept_lattice must not error against the real schema");
    let v: serde_json::Value =
        serde_json::from_str(&text_of(&result)).expect("fca body must be JSON");
    assert!(v["concepts"].is_array(), "concepts must be an array");
    assert_eq!(
        v["context"]["object_kind"].as_str(),
        Some("symbol"),
        "context echoes the requested object_kind"
    );
}

// =============================================================================
// CT-3 — csm_protocol_string_diagram (ADR-028 / Crucible ADR-010). The
// monoidal tensor decomposition of a real protocol's GlobalType. Seed one
// well-formed linear protocol (built with the csm constructors so the stored
// adjacent-tagged JSON is exactly what the tool decodes) and assert the
// decomposition: a linear protocol over the single pair {O,W} is one tensor
// factor.
// =============================================================================
#[tokio::test]
async fn tool_csm_protocol_string_diagram_against_seeded_protocol() {
    use pgmcp::csm::mpst::global::{end, interaction};
    use pgmcp::csm::role::Label;

    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    // O → W : t1_req . W → O : t1_done . end
    let g = interaction(
        "O",
        "W",
        Label::text("t1_req"),
        interaction("W", "O", Label::text("t1_done"), end()),
    );
    let gt = serde_json::to_value(&g).expect("serialize global_type");
    sqlx::query(
        "INSERT INTO csm_protocols (name, pattern_skill_id, global_type, participants, wellformed)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind("smoke_string_diagram")
    .bind("sequential")
    .bind(gt)
    .bind(vec!["O".to_string(), "W".to_string()])
    .bind(true)
    .execute(&pool)
    .await
    .expect("seed csm_protocols row");

    let result = server
        .call_tool_cli(
            "csm_protocol_string_diagram",
            json!({"protocol_name": "smoke_string_diagram"}),
        )
        .await
        .expect("csm_protocol_string_diagram must not error");
    let v: serde_json::Value =
        serde_json::from_str(&text_of(&result)).expect("string-diagram body must be JSON");
    assert_eq!(v["protocol"].as_str(), Some("smoke_string_diagram"));
    assert_eq!(
        v["n_tensor_factors"].as_i64(),
        Some(1),
        "O and W interact, so the linear protocol is a single tensor factor"
    );
}

// =============================================================================
// csm_protocol_to_tla — the deterministic GlobalType -> TLA+ encoder (the
// global-cursor model). Seed one well-formed linear protocol and assert the tool
// renders a faithful TLA+ module (a `g` cursor, a `fired` label map, a Spec) over
// the protocol's own labels. Also satisfies the Layer-D coverage net.
// =============================================================================
#[tokio::test]
async fn tool_csm_protocol_to_tla_against_seeded_protocol() {
    use pgmcp::csm::mpst::global::{end, interaction};
    use pgmcp::csm::role::Label;

    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    // O → W : t1_req . W → O : t1_done . end
    let g = interaction(
        "O",
        "W",
        Label::text("t1_req"),
        interaction("W", "O", Label::text("t1_done"), end()),
    );
    let gt = serde_json::to_value(&g).expect("serialize global_type");
    sqlx::query(
        "INSERT INTO csm_protocols (name, pattern_skill_id, global_type, participants, wellformed)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind("smoke_to_tla")
    .bind("sequential")
    .bind(gt)
    .bind(vec!["O".to_string(), "W".to_string()])
    .bind(true)
    .execute(&pool)
    .await
    .expect("seed csm_protocols row");

    let result = server
        .call_tool_cli(
            "csm_protocol_to_tla",
            json!({"protocol_name": "smoke_to_tla"}),
        )
        .await
        .expect("csm_protocol_to_tla must not error");
    let v: serde_json::Value =
        serde_json::from_str(&text_of(&result)).expect("to_tla body must be JSON");
    assert_eq!(v["protocol"].as_str(), Some("smoke_to_tla"));
    let tla = v["tla"].as_str().expect("tla field is a string");
    assert!(
        tla.contains("MODULE smoke_to_tla"),
        "emits a named module: {tla}"
    );
    assert!(tla.contains("Spec == Init"), "emits a Spec: {tla}");
    assert!(
        tla.contains("fired EXCEPT ![\"t1_req\"] = 1"),
        "encodes the protocol's labels into the fired map: {tla}"
    );
}
