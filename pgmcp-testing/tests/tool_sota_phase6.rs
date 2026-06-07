//! SOTA Phase 6 (security) integration tests.

use pgmcp_testing::pool_tool_helpers::{seed_file, seed_project, server_with_pool};
use pgmcp_testing::require_test_db;

fn text_of(result: &rmcp::model::CallToolResult) -> &str {
    for content in &result.content {
        if let rmcp::model::RawContent::Text(text) = &content.raw {
            return &text.text;
        }
    }
    panic!("tool returned no text content");
}

#[tokio::test(flavor = "multi_thread")]
async fn taint_analysis_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p6-ta", "/ws/p6-ta").await;
    seed_file(db.pool(), p, "/ws/p6-ta/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("taint_analysis", serde_json::json!({"project": "p6-ta"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn taint_analysis_normalizes_project_and_limit() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p6-ta-bounds", "/ws/p6-ta-bounds").await;
    seed_file(db.pool(), p, "/ws/p6-ta-bounds/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "taint_analysis",
            serde_json::json!({"project": " p6-ta-bounds ", "limit": 0}),
        )
        .await
        .expect("tool");
    let v: serde_json::Value = serde_json::from_str(text_of(&r)).expect("json response");

    assert_eq!(v["project"], "p6-ta-bounds");
    assert_eq!(v["limit"], 1);
    assert!(v["dataflow_findings"].as_array().expect("dataflow").len() <= 1);
    assert!(
        v["interprocedural_findings"]
            .as_array()
            .expect("interprocedural")
            .len()
            <= 1
    );
    assert!(v["heuristic_findings"].as_array().expect("heuristic").len() <= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn secret_detection_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p6-sd", "/ws/p6-sd").await;
    seed_file(db.pool(), p, "/ws/p6-sd/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("secret_detection", serde_json::json!({"project": "p6-sd"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn secret_detection_normalizes_bounds_and_streams_results() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p6-sd-bounds", "/ws/p6-sd-bounds").await;
    let f = seed_file(db.pool(), p, "/ws/p6-sd-bounds/a.rs", "a.rs").await;
    sqlx::query("UPDATE indexed_files SET content = $1, line_count = 2 WHERE id = $2")
        .bind(
            "const AWS = \"AKIAABCDEFGHIJKLMNOP\";\n\
             const OTHER = \"0123456789ABCDEFGHIJKLMNOPQRSTUV\";",
        )
        .bind(f)
        .execute(db.pool())
        .await
        .expect("seed secret content");

    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "secret_detection",
            serde_json::json!({
                "project": " p6-sd-bounds ",
                "min_entropy": 99.0,
                "limit": 0
            }),
        )
        .await
        .expect("tool");
    let v: serde_json::Value = serde_json::from_str(text_of(&r)).expect("json response");

    assert_eq!(v["project"], "p6-sd-bounds");
    assert_eq!(v["min_entropy"], 8.0);
    assert_eq!(v["limit"], 1);
    let findings = v["findings"].as_array().expect("findings array");
    assert_eq!(findings.len(), 1, "limit=0 must clamp before scanning");
    assert_eq!(findings[0]["file"], "a.rs");
    assert_eq!(findings[0]["kind"], "known-prefix");
}

#[tokio::test(flavor = "multi_thread")]
async fn crypto_misuse_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p6-cm", "/ws/p6-cm").await;
    seed_file(db.pool(), p, "/ws/p6-cm/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("crypto_misuse", serde_json::json!({"project": "p6-cm"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn crypto_misuse_clamps_limit_and_filters_ast_category_before_cap() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p6-cm-bounds", "/ws/p6-cm-bounds").await;
    let f = seed_file(db.pool(), p, "/ws/p6-cm-bounds/a.py", "a.py").await;
    sqlx::query(
        "UPDATE indexed_files SET language = 'python', content = $1, line_count = 3 WHERE id = $2",
    )
    .bind("import hashlib, pickle\nhashlib.md5(data)\npickle.loads(blob)\n")
    .bind(f)
    .execute(db.pool())
    .await
    .expect("seed python security content");

    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "crypto_misuse",
            serde_json::json!({"project": " p6-cm-bounds ", "limit": 0}),
        )
        .await
        .expect("tool");
    let v: serde_json::Value = serde_json::from_str(text_of(&r)).expect("json response");
    let ast = v["ast_findings"].as_array().expect("ast findings");

    assert_eq!(v["project"], "p6-cm-bounds");
    assert_eq!(v["limit"], 1);
    assert_eq!(ast.len(), 1);
    assert_eq!(ast[0]["rule"], "weak_md5");
}

#[tokio::test(flavor = "multi_thread")]
async fn unsafe_deserialization_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p6-ud", "/ws/p6-ud").await;
    seed_file(db.pool(), p, "/ws/p6-ud/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "unsafe_deserialization",
            serde_json::json!({"project": "p6-ud"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn unsafe_deserialization_filters_ast_category_before_cap() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p6-ud-bounds", "/ws/p6-ud-bounds").await;
    let f = seed_file(db.pool(), p, "/ws/p6-ud-bounds/a.py", "a.py").await;
    sqlx::query(
        "UPDATE indexed_files SET language = 'python', content = $1, line_count = 3 WHERE id = $2",
    )
    .bind("import hashlib, pickle\nhashlib.md5(data)\npickle.loads(blob)\n")
    .bind(f)
    .execute(db.pool())
    .await
    .expect("seed python security content");

    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "unsafe_deserialization",
            serde_json::json!({"project": " p6-ud-bounds ", "limit": 0}),
        )
        .await
        .expect("tool");
    let v: serde_json::Value = serde_json::from_str(text_of(&r)).expect("json response");
    let ast = v["ast_findings"].as_array().expect("ast findings");

    assert_eq!(v["project"], "p6-ud-bounds");
    assert_eq!(v["limit"], 1);
    assert_eq!(ast.len(), 1);
    assert_eq!(ast[0]["rule"], "pickle_load");
}

#[tokio::test(flavor = "multi_thread")]
async fn injection_candidates_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p6-ic", "/ws/p6-ic").await;
    seed_file(db.pool(), p, "/ws/p6-ic/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "injection_candidates",
            serde_json::json!({"project": "p6-ic"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn injection_candidates_rejects_unknown_kind() {
    let db = require_test_db!();
    seed_project(db.pool(), "p6-ic-kind", "/ws/p6-ic-kind").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "injection_candidates",
            serde_json::json!({"project": "p6-ic-kind", "kind": "sql-ish"}),
        )
        .await;

    match r {
        Ok(result) => assert_eq!(result.is_error, Some(true), "unknown kind must reject"),
        Err(_) => {}
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn injection_candidates_normalizes_kind_project_and_limit() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p6-ic-bounds", "/ws/p6-ic-bounds").await;
    seed_file(db.pool(), p, "/ws/p6-ic-bounds/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "injection_candidates",
            serde_json::json!({"project": " p6-ic-bounds ", "kind": " sql ", "limit": 0}),
        )
        .await
        .expect("tool");
    let v: serde_json::Value = serde_json::from_str(text_of(&r)).expect("json response");

    assert_eq!(v["project"], "p6-ic-bounds");
    assert_eq!(v["kind"], "sql");
    assert_eq!(v["limit"], 1);
    assert!(
        v["injection_findings"]
            .as_array()
            .expect("injection findings")
            .len()
            <= 1
    );
    assert!(
        v["heuristic_matches"]
            .as_array()
            .expect("heuristic matches")
            .len()
            <= 1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unprotected_routes_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p6-ur", "/ws/p6-ur").await;
    seed_file(db.pool(), p, "/ws/p6-ur/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "unprotected_routes",
            serde_json::json!({"project": "p6-ur"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn cve_supply_chain_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p6-cve", "/ws/p6-cve").await;
    seed_file(db.pool(), p, "/ws/p6-cve/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("cve_supply_chain", serde_json::json!({"project": "p6-cve"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}
