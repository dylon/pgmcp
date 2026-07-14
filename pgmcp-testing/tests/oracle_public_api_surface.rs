//! Real-Postgres correctness oracle for `public_api_surface`.

use pgmcp_testing::pool_tool_helpers::{
    seed_file, seed_file_symbol, seed_project, server_with_pool,
};
use pgmcp_testing::require_test_db;

use crate::common::text_of;

#[tokio::test]
async fn public_api_surface_summary_is_not_limited_and_full_is_limited() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = seed_project(&pool, "api-surface-proj", "/ws/api-surface-proj").await;
    let file_id = seed_file(
        &pool,
        project_id,
        "/ws/api-surface-proj/src/lib.rs",
        "src/lib.rs",
    )
    .await;
    seed_file_symbol(&pool, file_id, "alpha", "function", 1, Some("public")).await;
    seed_file_symbol(&pool, file_id, "Beta", "struct", 20, Some("public")).await;
    seed_file_symbol(&pool, file_id, "hidden", "function", 40, Some("private")).await;

    let server = server_with_pool(pool);

    let summary = server
        .call_tool_cli(
            "public_api_surface",
            serde_json::json!({
                "project": " api-surface-proj ",
                "format": "summary",
                "limit": 1
            }),
        )
        .await
        .expect("summary call");
    let summary_v: serde_json::Value = serde_json::from_str(&text_of(&summary)).expect("json");
    assert_eq!(summary_v["project"].as_str(), Some("api-surface-proj"));
    assert_eq!(
        summary_v["total_public"].as_i64(),
        Some(2),
        "summary must count all public symbols, not the full-format limit"
    );
    assert_eq!(summary_v["by_kind"]["function"].as_i64(), Some(1));
    assert_eq!(summary_v["by_kind"]["struct"].as_i64(), Some(1));

    let full = server
        .call_tool_cli(
            "public_api_surface",
            serde_json::json!({
                "project": "api-surface-proj",
                "format": "full",
                "limit": -10
            }),
        )
        .await
        .expect("full call");
    let full_v: serde_json::Value = serde_json::from_str(&text_of(&full)).expect("json");
    assert_eq!(full_v["total_public"].as_i64(), Some(2));
    assert_eq!(full_v["limit"].as_i64(), Some(1));
    assert_eq!(full_v["returned"].as_u64(), Some(1));
    assert_eq!(full_v["symbols"].as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn public_api_surface_rejects_bad_inputs_and_duplicate_projects() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    seed_project(&pool, "format-api-surface", "/ws/api-surface-format").await;
    seed_project(&pool, "dup-api-surface", "/ws/api-surface-a").await;
    seed_project(&pool, "dup-api-surface", "/ws/api-surface-b").await;
    let server = server_with_pool(pool);

    assert!(
        server
            .call_tool_cli("public_api_surface", serde_json::json!({"project": "   "}))
            .await
            .is_err(),
        "blank project must fail closed"
    );
    assert!(
        server
            .call_tool_cli(
                "public_api_surface",
                serde_json::json!({"project": "dup-api-surface"}),
            )
            .await
            .is_err(),
        "duplicate project display names must fail closed"
    );
    assert!(
        server
            .call_tool_cli(
                "public_api_surface",
                serde_json::json!({"project": "format-api-surface", "format": "xml"}),
            )
            .await
            .is_err(),
        "unknown format must fail closed"
    );
}
