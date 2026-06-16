//! Real-Postgres oracle for `topic_trends`.
//!
//! With no history the tool reports "insufficient"; given a synthetic
//! two-snapshot size history (a growing theme), mode=longitudinal surfaces it as
//! emerging. (The trend math itself is unit-tested in `topic_analysis::trends`.)

mod common;

use common::server_with_pool;
use pgmcp_testing::require_test_db;

fn text_of(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .next()
        .expect("text content present")
}

#[tokio::test]
async fn topic_trends_insufficient_then_emerging() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    // No history yet → longitudinal mode reports insufficient.
    let result = server
        .call_tool_cli("topic_trends", serde_json::json!({"mode": "longitudinal"}))
        .await
        .expect("topic_trends call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert!(
        v["note"]
            .as_str()
            .map(|s| s.contains("insufficient"))
            .unwrap_or(false),
        "no history should report insufficient: {v}"
    );

    // Seed a two-snapshot history: a global theme grows 10 → 30 over a week.
    let history = serde_json::json!([
        {"at": "2026-01-01T00:00:00+00:00", "topics": [
            {"scope": "global", "topic_id": 1, "label": "rising", "chunk_count": 10}
        ]},
        {"at": "2026-01-08T00:00:00+00:00", "topics": [
            {"scope": "global", "topic_id": 1, "label": "rising", "chunk_count": 30}
        ]}
    ]);
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ('topics_size_history', $1)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(history.to_string())
    .execute(&pool)
    .await
    .expect("seed history");

    let result = server
        .call_tool_cli(
            "topic_trends",
            serde_json::json!({"mode": "longitudinal", "scope": "global"}),
        )
        .await
        .expect("topic_trends call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let emerging = v["emerging"].as_array().expect("emerging array");
    assert_eq!(emerging.len(), 1, "the growing theme is emerging: {v}");
    assert_eq!(emerging[0]["label"], "rising");
    assert!(
        emerging[0]["slope_per_week"].as_f64().expect("slope") > 0.0,
        "rising theme has positive slope: {v}"
    );

    // Unknown mode fails closed.
    let err = server
        .call_tool_cli("topic_trends", serde_json::json!({"mode": "bogus"}))
        .await
        .expect_err("unknown mode must fail");
    assert!(err.to_string().contains("mode must be"), "got: {err}");
}
