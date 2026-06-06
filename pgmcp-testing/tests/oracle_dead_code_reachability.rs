//! Focused oracle coverage for `dead_code_reachability`.

use std::collections::HashSet;

use pgmcp_testing::pool_tool_helpers::{
    seed_file, seed_file_symbol, seed_project, server_with_pool,
};
use pgmcp_testing::require_test_db;

fn text_of(result: &rmcp::model::CallToolResult) -> &str {
    for content in &result.content {
        if let rmcp::model::RawContent::Text(text) = &content.raw {
            return &text.text;
        }
    }
    panic!("tool returned no text content");
}

async fn insert_call(
    pool: &sqlx::PgPool,
    source_file_id: i64,
    source_symbol_id: i64,
    target_symbol_id: i64,
    target_raw: &str,
    source_line: i32,
    resolution_kind: &str,
) {
    let confidence = match resolution_kind {
        "exact_in_file" | "exact_via_import" => 1.0_f32,
        "bare_name_in_project" => 0.5_f32,
        _ => 0.0_f32,
    };
    sqlx::query(
        "INSERT INTO symbol_references
             (source_file_id, source_symbol_id, target_symbol_id, target_raw,
              ref_kind, source_line, resolution_kind, resolution_confidence)
         VALUES ($1, $2, $3, $4, 'call', $5, $6, $7)
         ON CONFLICT (source_file_id, source_line, target_raw, ref_kind) DO UPDATE
             SET source_symbol_id = EXCLUDED.source_symbol_id,
                 target_symbol_id = EXCLUDED.target_symbol_id,
                 resolution_kind = EXCLUDED.resolution_kind,
                 resolution_confidence = EXCLUDED.resolution_confidence",
    )
    .bind(source_file_id)
    .bind(source_symbol_id)
    .bind(target_symbol_id)
    .bind(target_raw)
    .bind(source_line)
    .bind(resolution_kind)
    .bind(confidence)
    .execute(pool)
    .await
    .expect("insert symbol reference");
}

fn dead_candidate_names(v: &serde_json::Value) -> HashSet<String> {
    v["dead_candidates"]
        .as_array()
        .expect("dead candidates")
        .iter()
        .map(|candidate| candidate["name"].as_str().expect("name").to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn dead_code_reachability_rejects_stale_edges_and_clamps_limit() {
    let db = require_test_db!();
    let pool = db.pool();

    let project = seed_project(pool, "dcr-main", "/ws/dcr-main").await;
    let file = seed_file(pool, project, "/ws/dcr-main/src/lib.rs", "src/lib.rs").await;
    let root = seed_file_symbol(pool, file, "main", "function", 1, Some("public")).await;
    let reachable = seed_file_symbol(pool, file, "reachable", "function", 5, None).await;
    let via_bare = seed_file_symbol(pool, file, "via_bare", "function", 9, None).await;
    let dead = seed_file_symbol(pool, file, "dead", "function", 13, None).await;

    let other_project = seed_project(pool, "dcr-other", "/ws/dcr-other").await;
    let other_file = seed_file(
        pool,
        other_project,
        "/ws/dcr-other/src/lib.rs",
        "src/lib.rs",
    )
    .await;
    let foreign =
        seed_file_symbol(pool, other_file, "foreign", "function", 1, Some("public")).await;

    insert_call(
        pool,
        file,
        root,
        reachable,
        "reachable",
        20,
        "exact_in_file",
    )
    .await;
    insert_call(
        pool,
        file,
        root,
        via_bare,
        "via_bare",
        21,
        "bare_name_in_project",
    )
    .await;

    // Two stale rows that previously allowed BFS to walk out through another
    // project's symbol and back into this project, incorrectly hiding `dead`.
    insert_call(pool, file, root, foreign, "foreign", 22, "exact_in_file").await;
    insert_call(pool, file, foreign, dead, "dead", 23, "exact_in_file").await;

    let server = server_with_pool(pool.clone());
    let result = server
        .call_tool_cli(
            "dead_code_reachability",
            serde_json::json!({"project": " dcr-main ", "limit": 50_000}),
        )
        .await
        .expect("dead_code_reachability");
    let v: serde_json::Value = serde_json::from_str(text_of(&result)).expect("json");
    assert_eq!(v["project"].as_str(), Some("dcr-main"));
    assert_eq!(v["limit"].as_u64(), Some(1_000));
    assert_eq!(v["include_tests"].as_bool(), Some(false));
    assert_eq!(v["include_bare_name"].as_bool(), Some(false));

    let names = dead_candidate_names(&v);
    assert!(
        names.contains("dead"),
        "stale cross-project edges must not make private code reachable: {v:#}"
    );
    assert!(
        names.contains("via_bare"),
        "bare-name edges require explicit opt-in: {v:#}"
    );
    assert!(
        !names.contains("reachable"),
        "exact in-project edge should mark symbol reachable: {v:#}"
    );
    assert!(
        !v.to_string().contains("foreign"),
        "foreign project symbols must not leak into the response: {v:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dead_code_reachability_include_flags_are_exact() {
    let db = require_test_db!();
    let pool = db.pool();

    let project = seed_project(pool, "dcr-flags", "/ws/dcr-flags").await;
    let file = seed_file(pool, project, "/ws/dcr-flags/src/lib.rs", "src/lib.rs").await;
    let test_file = seed_file(
        pool,
        project,
        "/ws/dcr-flags/tests/integration.rs",
        "tests/integration.rs",
    )
    .await;

    let root = seed_file_symbol(pool, file, "main", "function", 1, Some("public")).await;
    let test_root =
        seed_file_symbol(pool, test_file, "test_entry", "function", 1, Some("public")).await;
    let test_only = seed_file_symbol(pool, file, "test_only", "function", 5, None).await;
    let bare_only = seed_file_symbol(pool, file, "bare_only", "function", 9, None).await;

    insert_call(
        pool,
        test_file,
        test_root,
        test_only,
        "test_only",
        10,
        "exact_in_file",
    )
    .await;
    insert_call(
        pool,
        file,
        root,
        bare_only,
        "bare_only",
        11,
        "bare_name_in_project",
    )
    .await;

    let server = server_with_pool(pool.clone());
    let strict = server
        .call_tool_cli(
            "dead_code_reachability",
            serde_json::json!({"project": "dcr-flags"}),
        )
        .await
        .expect("strict dead_code_reachability");
    let strict_v: serde_json::Value = serde_json::from_str(text_of(&strict)).expect("json");
    let strict_names = dead_candidate_names(&strict_v);
    assert!(strict_names.contains("test_only"), "{strict_v:#}");
    assert!(strict_names.contains("bare_only"), "{strict_v:#}");

    let relaxed = server
        .call_tool_cli(
            "dead_code_reachability",
            serde_json::json!({
                "project": "dcr-flags",
                "include_tests": true,
                "include_bare_name": true,
            }),
        )
        .await
        .expect("relaxed dead_code_reachability");
    let relaxed_v: serde_json::Value = serde_json::from_str(text_of(&relaxed)).expect("json");
    let relaxed_names = dead_candidate_names(&relaxed_v);
    assert!(!relaxed_names.contains("test_only"), "{relaxed_v:#}");
    assert!(!relaxed_names.contains("bare_only"), "{relaxed_v:#}");
}
