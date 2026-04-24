//! Real-Postgres correctness oracle for `bug_prediction`.
//! Boost util/util.rs's fix_commit_ratio so its bug_score (which
//! weights fix_ratio at 3.0 — the largest coefficient) dominates.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::require_test_db;

#[tokio::test]
async fn bug_prediction_ranks_high_churn_high_fix_ratio_file_first() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let h = seed_graph_corpus(&pool).await;

    sqlx::query("UPDATE file_metrics SET fix_commit_ratio = 0.6 WHERE file_id = $1")
        .bind(h.files["util"].0)
        .execute(&pool)
        .await
        .expect("bump fix_ratio");

    let server = server_with_pool(pool);
    let result = server
        .call_tool_cli(
            "bug_prediction",
            serde_json::json!({"project": "graph-proj"}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let files = v["files"].as_array().expect("files");
    assert!(!files.is_empty(), "bug_prediction returned no files");
    assert_eq!(
        files[0]["path"].as_str(),
        Some("util/util.rs"),
        "util/util.rs (high churn + high fix_ratio) must rank first; got {}",
        files[0]["path"]
    );
}
