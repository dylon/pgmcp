//! Integration tests for topic apps #3 (topic_drift_warning) and #8
//! (topic_ownership_forecast). Coverage gate via call_tool_cli.

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

fn body(r: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&text_of(r)).expect("tool body must be JSON")
}

#[tokio::test]
async fn topic_drift_warning_flags_growth() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    // Two size snapshots: topic 9991 grows 10 → 30 (+200%).
    let hist = r#"[
        {"at":"2026-01-01T00:00:00Z","topics":[{"scope":"global","topic_id":9991,"label":"growth-theme","chunk_count":10}]},
        {"at":"2026-02-01T00:00:00Z","topics":[{"scope":"global","topic_id":9991,"label":"growth-theme","chunk_count":30}]}
    ]"#;
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ('topics_size_history', $1)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(hist)
    .execute(&pool)
    .await
    .expect("seed history");

    let res = body(
        &server
            .call_tool_cli("topic_drift_warning", json!({"min_pct_change": 0.5}))
            .await
            .expect("topic_drift_warning"),
    );
    let drifts = res["drifting_topics"].as_array().expect("drifts");
    let d = drifts
        .iter()
        .find(|d| d["topic_id"].as_i64() == Some(9991))
        .expect("growth-theme flagged");
    assert_eq!(d["direction"], "emerging", "{d}");
    assert!(d["pct_change"].as_f64().unwrap() >= 1.9, "{d}");
}

#[tokio::test]
async fn topic_ownership_forecast_flags_single_owner() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ('/ws/own','/ws/own/p','ownproj')
         ON CONFLICT (path) DO UPDATE SET name='ownproj' RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("project");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at)
         VALUES ($1, '/ws/own/p/a.rs', 'a.rs', 'rust', 50, 'x', 'ownhash', 5, NOW())
         ON CONFLICT (path) DO UPDATE SET content='x' RETURNING id",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .expect("file");
    let topic_id: i32 = sqlx::query_scalar(
        "INSERT INTO code_topics (scope, cluster_index, label, chunk_count, file_count, project_count, project_names)
         VALUES ('global', 8810, 'owned-topic', 2, 1, 1, ARRAY['ownproj'])
         ON CONFLICT (scope, cluster_index) DO UPDATE SET label='owned-topic' RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("topic");
    for i in 0..2 {
        let cid: i64 = sqlx::query_scalar(
            "INSERT INTO file_chunks (file_id, chunk_index, content, start_line, end_line, blame_author)
             VALUES ($1, $2, 'code', 1, 2, 'alice')
             ON CONFLICT (file_id, chunk_index) DO UPDATE SET blame_author='alice' RETURNING id",
        )
        .bind(file_id)
        .bind(i)
        .fetch_one(&pool)
        .await
        .expect("chunk");
        sqlx::query(
            "INSERT INTO chunk_topic_assignments (chunk_id, topic_id, membership_score) VALUES ($1,$2,1.0)
             ON CONFLICT (chunk_id, topic_id) DO NOTHING",
        )
        .bind(cid)
        .bind(topic_id)
        .execute(&pool)
        .await
        .expect("assignment");
    }

    let res = body(
        &server
            .call_tool_cli("topic_ownership_forecast", json!({}))
            .await
            .expect("topic_ownership_forecast"),
    );
    let t = res["topics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["topic_id"].as_i64() == Some(topic_id as i64))
        .expect("owned-topic present");
    assert_eq!(t["bus_factor"].as_i64(), Some(1), "single author: {t}");
    assert_eq!(t["single_owner_risk"], true, "{t}");
}
