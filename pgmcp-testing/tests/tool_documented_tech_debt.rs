//! Integration test for `documented_tech_debt` MCP tool.
//!
//! Seeds project files with documented debt markers and verifies parameter
//! normalization, closed-set validation, scoping, and JSON output shape. The
//! literal `call_tool_cli("documented_tech_debt", …)` satisfies the
//! `every_dispatched_tool_has_an_integration_test` coverage gate.

use crate::common::text_of;
use pgmcp_testing::pool_tool_helpers::{seed_file, seed_project, server_with_pool};
use pgmcp_testing::require_test_db;
use serde_json::Value;

#[tokio::test(flavor = "multi_thread")]
async fn documented_tech_debt_summary_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "dtd-p", "/ws/dtd-p").await;
    seed_file(db.pool(), p, "/ws/dtd-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "documented_tech_debt",
            serde_json::json!({"project": " dtd-p ", "format": " summary ", "limit": 5000}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
    let v: Value = serde_json::from_str(&text_of(&r)).expect("documented debt JSON");
    assert_eq!(v["summary"]["project"].as_str(), Some("dtd-p"));
    assert_eq!(v["summary"]["filters"]["format"].as_str(), Some("summary"));
    assert_eq!(v["summary"]["filters"]["limit"].as_u64(), Some(1000));

    assert!(
        server
            .call_tool_cli(
                "documented_tech_debt",
                serde_json::json!({"project": "dtd-p", "format": "html"}),
            )
            .await
            .is_err(),
        "unknown output format is rejected"
    );
    assert!(
        server
            .call_tool_cli(
                "documented_tech_debt",
                serde_json::json!({"project": "dtd-p", "severity": "urgent"}),
            )
            .await
            .is_err(),
        "unknown severity is rejected"
    );
    assert!(
        server
            .call_tool_cli(
                "documented_tech_debt",
                serde_json::json!({"project": "dtd-p", "category": "misc"}),
            )
            .await
            .is_err(),
        "unknown category is rejected"
    );
    assert!(
        server
            .call_tool_cli(
                "documented_tech_debt",
                serde_json::json!({"project": "dtd-p", "min_age_days": -1}),
            )
            .await
            .is_err(),
        "negative min_age_days is rejected"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn documented_tech_debt_full_format_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "dtd-full", "/ws/dtd-full").await;
    let file_id = seed_file(db.pool(), p, "/ws/dtd-full/a.rs", "a.rs").await;
    sqlx::query(
        "UPDATE indexed_files
         SET content = $2, line_count = 3, size_bytes = length($2)
         WHERE id = $1",
    )
    .bind(file_id)
    .bind("fn f() {}\n// TODO(#123): finish this path\nfn g() {}\n")
    .execute(db.pool())
    .await
    .expect("seed debt content");
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "documented_tech_debt",
            serde_json::json!({"project": "dtd-full", "format": " full ", "category": " comments "}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
    let v: Value = serde_json::from_str(&text_of(&r)).expect("documented debt full JSON");
    assert_eq!(v["summary"]["filters"]["format"].as_str(), Some("full"));
    assert_eq!(
        v["summary"]["filters"]["category"].as_str(),
        Some("comments")
    );
    assert_eq!(v["summary"]["total_markers"].as_u64(), Some(1));
    assert_eq!(v["findings"][0]["kind"].as_str(), Some("TODO"));
    assert_eq!(v["findings"][0]["issue_refs"][0].as_str(), Some("#123"));
}

#[tokio::test(flavor = "multi_thread")]
async fn documented_tech_debt_severity_filter_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "dtd-sev", "/ws/dtd-sev").await;
    let file_id = seed_file(db.pool(), p, "/ws/dtd-sev/a.rs", "a.rs").await;
    sqlx::query(
        "UPDATE indexed_files
         SET content = $2, line_count = 3, size_bytes = length($2)
         WHERE id = $1",
    )
    .bind(file_id)
    .bind("fn f() {}\n// FIXME: race on shared queue\n// TODO: later polish\n")
    .execute(db.pool())
    .await
    .expect("seed severity content");
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "documented_tech_debt",
            serde_json::json!({"project": "dtd-sev", "severity": " HIGH ", "format": " full "}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
    let v: Value = serde_json::from_str(&text_of(&r)).expect("documented debt severity JSON");
    assert_eq!(v["summary"]["filters"]["severity"].as_str(), Some("high"));
    assert_eq!(v["summary"]["total_markers"].as_u64(), Some(1));
    assert_eq!(v["findings"][0]["kind"].as_str(), Some("FIXME"));
    assert_eq!(v["findings"][0]["severity"].as_str(), Some("high"));
}
