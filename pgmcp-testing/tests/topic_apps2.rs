//! Integration tests for topic apps #4 (topic_scoped_search) and #11
//! (topic_quality_forecast). Drives both dispatched tools via call_tool_cli
//! (Layer-D coverage gate).

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

fn body(r: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&text_of(r)).expect("tool body must be JSON")
}

fn vec1024(seed: usize) -> pgvector::Vector {
    let mut v = vec![0.0f32; 1024];
    v[seed % 1024] = 1.0;
    v[(seed + 1) % 1024] = 0.5;
    pgvector::Vector::from(v)
}

#[tokio::test]
async fn topic_scoped_search_restricts_to_topic() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ('/ws/ts','/ws/ts/p','tsproj')
         ON CONFLICT (path) DO UPDATE SET name='tsproj' RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("project");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at)
         VALUES ($1, '/ws/ts/p/auth.rs', 'auth.rs', 'rust', 100, 'x', 'tshash', 10, NOW())
         ON CONFLICT (path) DO UPDATE SET content='x' RETURNING id",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .expect("file");
    let topic_id: i32 = sqlx::query_scalar(
        "INSERT INTO code_topics (scope, cluster_index, label, chunk_count, file_count, project_count, project_names)
         VALUES ('global', 7700, 'authstuff', 2, 1, 1, ARRAY['tsproj'])
         ON CONFLICT (scope, cluster_index) DO UPDATE SET label='authstuff' RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("topic");
    for i in 0..2 {
        let chunk_id: i64 = sqlx::query_scalar(
            "INSERT INTO file_chunks (file_id, chunk_index, content, start_line, end_line, embedding_v2)
             VALUES ($1, $2, 'fn login() { authenticate() }', 1, 5, $3)
             ON CONFLICT (file_id, chunk_index) DO UPDATE SET embedding_v2 = $3 RETURNING id",
        )
        .bind(file_id)
        .bind(i as i32)
        .bind(vec1024(i))
        .fetch_one(&pool)
        .await
        .expect("chunk");
        sqlx::query(
            "INSERT INTO chunk_topic_assignments (chunk_id, topic_id, membership_score)
             VALUES ($1, $2, 1.0) ON CONFLICT (chunk_id, topic_id) DO NOTHING",
        )
        .bind(chunk_id)
        .bind(topic_id)
        .execute(&pool)
        .await
        .expect("assignment");
    }

    let res = body(
        &server
            .call_tool_cli(
                "topic_scoped_search",
                json!({"query": "login authentication", "topic_label": "authstuff"}),
            )
            .await
            .expect("topic_scoped_search"),
    );
    assert_eq!(res["topic_id"].as_i64(), Some(topic_id as i64));
    assert!(
        res["count"].as_i64().unwrap() >= 1,
        "chunks in the topic must be returned: {res}"
    );
}

#[tokio::test]
async fn topic_quality_forecast_trends_over_history() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    // A rising architecture-quality history (global scope: project_id NULL).
    for (days_ago, gpa) in [(20_i32, 0.40_f32), (10, 0.50), (0, 0.60)] {
        sqlx::query(
            "INSERT INTO quality_report_history (project_id, computed_at, architecture_gpa, overall_gpa, raw_summary)
             VALUES (NULL, NOW() - ($1 || ' days')::interval, $2, $2, '{}'::jsonb)",
        )
        .bind(days_ago.to_string())
        .bind(gpa)
        .execute(&pool)
        .await
        .expect("history row");
    }

    let res = body(
        &server
            .call_tool_cli("topic_quality_forecast", json!({"threshold": 0.8}))
            .await
            .expect("topic_quality_forecast"),
    );
    assert!(res["points"].as_i64().unwrap() >= 3, "{res}");
    assert_eq!(
        res["trend"], "improving",
        "rising history → improving: {res}"
    );
    assert!(
        res["slope_per_day"].as_f64().unwrap() > 0.0,
        "positive slope: {res}"
    );
}

#[tokio::test]
async fn doc_code_topic_alignment_runs() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());
    let res = body(
        &server
            .call_tool_cli("doc_code_topic_alignment", json!({}))
            .await
            .expect("doc_code_topic_alignment"),
    );
    // JSD is always present and in [0,1] (0 when there is no data).
    let jsd = res["jensen_shannon_divergence"].as_f64().expect("jsd");
    assert!((0.0..=1.0).contains(&jsd), "jsd in range: {res}");
    assert!(res["topics"].is_array());
}
