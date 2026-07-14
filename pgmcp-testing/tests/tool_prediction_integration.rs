//! Phase 6: real-DB integration tests for the Prediction tool category.
//! `bug_prediction`, `technical_debt_analysis`, `anomaly_detection`.

use crate::common::text_of;
use pgmcp_testing::pool_tool_helpers::{
    seed_file, seed_file_symbol, seed_project, server_with_pool,
};
use pgmcp_testing::require_test_db;
use serde_json::Value;

#[tokio::test(flavor = "multi_thread")]
async fn bug_prediction_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "bp-p", "/ws/bp-p").await;
    seed_file(db.pool(), p, "/ws/bp-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli("bug_prediction", serde_json::json!({"project": "bp-p"}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn technical_debt_analysis_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "tdebt-p", "/ws/tdebt-p").await;
    seed_file(db.pool(), p, "/ws/tdebt-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "technical_debt_analysis",
            serde_json::json!({"project": "tdebt-p"}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn anomaly_detection_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "anom-p", "/ws/anom-p").await;
    seed_file(db.pool(), p, "/ws/anom-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "anomaly_detection",
            serde_json::json!({"project": "anom-p"}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn code_on_fire_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "cof-p", "/ws/cof-p").await;
    let file_id = seed_file(db.pool(), p, "/ws/cof-p/a.rs", "a.rs").await;
    sqlx::query(
        "INSERT INTO file_metrics
            (file_id, project_id, churn_rate, commit_count)
         VALUES ($1, $2, 0.8, 12)
         ON CONFLICT (file_id) DO UPDATE
             SET project_id = EXCLUDED.project_id,
                 churn_rate = EXCLUDED.churn_rate,
                 commit_count = EXCLUDED.commit_count",
    )
    .bind(file_id)
    .bind(p)
    .execute(db.pool())
    .await
    .expect("file_metrics");
    let fn_id = seed_file_symbol(db.pool(), file_id, "burning_function", "function", 1, None).await;
    sqlx::query(
        "INSERT INTO function_metrics
            (function_id, file_id, project_id, cyclomatic, cognitive, maintainability_index, npath)
         VALUES ($1, $2, $3, 12, 18, 35.0, 40)
         ON CONFLICT (function_id) DO UPDATE
             SET cyclomatic = EXCLUDED.cyclomatic,
                 cognitive = EXCLUDED.cognitive,
                 maintainability_index = EXCLUDED.maintainability_index,
                 npath = EXCLUDED.npath",
    )
    .bind(fn_id)
    .bind(file_id)
    .bind(p)
    .execute(db.pool())
    .await
    .expect("function_metrics");

    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "code_on_fire",
            serde_json::json!({
                "project": " cof-p ",
                "mode": " union ",
                "limit": 500,
                "churn_quartile": 0.0,
                "complexity_quartile": 0.0
            }),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
    let v: Value = serde_json::from_str(&text_of(&result)).expect("code_on_fire JSON");
    assert_eq!(v["project"].as_str(), Some("cof-p"));
    assert_eq!(v["mode"].as_str(), Some("union"));
    assert_eq!(v["returned"].as_u64(), Some(1));
    assert_eq!(
        v["results"][0]["function"].as_str(),
        Some("burning_function")
    );

    assert!(
        server
            .call_tool_cli(
                "code_on_fire",
                serde_json::json!({"project": "cof-p", "mode": "sideways"}),
            )
            .await
            .is_err(),
        "unknown mode is rejected"
    );
    assert!(
        server
            .call_tool_cli(
                "code_on_fire",
                serde_json::json!({"project": "cof-p", "churn_quartile": 1.5}),
            )
            .await
            .is_err(),
        "out-of-range churn quartile is rejected"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn code_on_fire_ignores_cross_project_metric_rows() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "cof-stale", "/ws/cof-stale").await;
    let other = seed_project(db.pool(), "cof-other", "/ws/cof-other").await;
    let file_id = seed_file(db.pool(), p, "/ws/cof-stale/a.rs", "a.rs").await;
    sqlx::query(
        "INSERT INTO file_metrics
            (file_id, project_id, churn_rate, commit_count)
         VALUES ($1, $2, 0.9, 99)
         ON CONFLICT (file_id) DO UPDATE
             SET project_id = EXCLUDED.project_id,
                 churn_rate = EXCLUDED.churn_rate,
                 commit_count = EXCLUDED.commit_count",
    )
    .bind(file_id)
    .bind(other)
    .execute(db.pool())
    .await
    .expect("stale file_metrics");
    let fn_id = seed_file_symbol(
        db.pool(),
        file_id,
        "stale_metric_function",
        "function",
        1,
        None,
    )
    .await;
    sqlx::query(
        "INSERT INTO function_metrics
            (function_id, file_id, project_id, cyclomatic, cognitive, maintainability_index, npath)
         VALUES ($1, $2, $3, 99, 99, 1.0, 99)
         ON CONFLICT (function_id) DO UPDATE
             SET file_id = EXCLUDED.file_id,
                 project_id = EXCLUDED.project_id,
                 cyclomatic = EXCLUDED.cyclomatic,
                 cognitive = EXCLUDED.cognitive,
                 maintainability_index = EXCLUDED.maintainability_index,
                 npath = EXCLUDED.npath",
    )
    .bind(fn_id)
    .bind(file_id)
    .bind(other)
    .execute(db.pool())
    .await
    .expect("stale function_metrics");

    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "code_on_fire",
            serde_json::json!({"project": "cof-stale", "mode": "max"}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
    let v: Value = serde_json::from_str(&text_of(&result)).expect("code_on_fire JSON");
    assert_eq!(v["project"].as_str(), Some("cof-stale"));
    assert_eq!(v["returned"].as_u64(), Some(0));
    assert_eq!(v["results"].as_array().map(Vec::len), Some(0));
}
