//! SOTA Phase 5 (concurrency / safety / performance) integration tests.

use pgmcp_testing::pool_tool_helpers::{seed_file, seed_project, server_with_pool};
use pgmcp_testing::require_test_db;
use uuid::Uuid;

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

#[tokio::test(flavor = "multi_thread")]
async fn lockset_races_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-lockset", "/ws/p5-lockset").await;
    seed_file(db.pool(), p, "/ws/p5-lockset/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "lockset_races",
            serde_json::json!({"project": "p5-lockset"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn unsafe_clusters_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-uc", "/ws/p5-uc").await;
    let a = seed_file(db.pool(), p, "/ws/p5-uc/a.rs", "a.rs").await;
    let b = seed_file(db.pool(), p, "/ws/p5-uc/b.rs", "b.rs").await;
    sqlx::query("UPDATE indexed_files SET content = $1, line_count = 4 WHERE id = $2")
        .bind("unsafe fn a() {}\nunsafe fn b() {}\nfn safe() {}\n")
        .bind(a)
        .execute(db.pool())
        .await
        .expect("seed unsafe a");
    sqlx::query("UPDATE indexed_files SET content = $1, line_count = 2 WHERE id = $2")
        .bind("unsafe fn c() {}\n")
        .bind(b)
        .execute(db.pool())
        .await
        .expect("seed unsafe b");
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "unsafe_clusters",
            serde_json::json!({"project": " p5-uc ", "limit": 1}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
    let v: serde_json::Value = serde_json::from_str(&text_of(&r)).expect("unsafe JSON");
    assert_eq!(v["project"].as_str(), Some("p5-uc"));
    assert_eq!(v["limit"].as_u64(), Some(1));
    assert_eq!(v["total_unsafe_blocks"].as_u64(), Some(3));
    let files = v["files"].as_array().expect("files");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["file"].as_str(), Some("a.rs"));
    assert_eq!(files[0]["unsafe_blocks"].as_u64(), Some(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn unsafe_clusters_rejects_ambiguous_project_name() {
    let db = require_test_db!();
    let name = format!("duplicate-unsafe-{}", Uuid::now_v7().simple());
    for suffix in ["a", "b"] {
        sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
            .bind(format!("/ws/{suffix}"))
            .bind(format!("/ws/{suffix}/{name}"))
            .bind(&name)
            .execute(db.pool())
            .await
            .expect("project");
    }

    let server = server_with_pool(db.pool().clone());
    let err = server
        .call_tool_cli("unsafe_clusters", serde_json::json!({"project": name}))
        .await
        .expect_err("duplicate project display names must fail closed");

    assert!(
        err.to_string().contains("ambiguous project name"),
        "unexpected unsafe_clusters ambiguity error: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn panic_paths_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-pp", "/ws/p5-pp").await;
    seed_file(db.pool(), p, "/ws/p5-pp/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("panic_paths", serde_json::json!({"project": "p5-pp"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn deadlock_candidates_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-dl", "/ws/p5-dl").await;
    seed_file(db.pool(), p, "/ws/p5-dl/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "deadlock_candidates",
            serde_json::json!({"project": "p5-dl"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn send_sync_violations_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-ss", "/ws/p5-ss").await;
    seed_file(db.pool(), p, "/ws/p5-ss/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "send_sync_violations",
            serde_json::json!({"project": "p5-ss"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn send_sync_violations_detects_patterns_and_clamps_limit() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-ss-detect", "/ws/p5-ss-detect").await;
    let file_id = seed_file(db.pool(), p, "/ws/p5-ss-detect/a.rs", "a.rs").await;
    sqlx::query(
        "UPDATE indexed_files SET content = $2, language = 'rust'
         WHERE id = $1",
    )
    .bind(file_id)
    .bind(
        r#"
use std::cell::RefCell;
use std::sync::Arc;

static mut GLOBAL: usize = 0;

struct UnsafeBox(*mut usize);
unsafe impl Send for UnsafeBox {}

fn main() {
    let _cell = Arc::<RefCell<usize>>::new(RefCell::new(1));
}
"#,
    )
    .execute(db.pool())
    .await
    .expect("update fixture content");

    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "send_sync_violations",
            serde_json::json!({
                "project": "  p5-ss-detect  ",
                "limit": -10,
            }),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
    let v: serde_json::Value = serde_json::from_str(&text_of(&r)).expect("json");
    assert_eq!(v["project"].as_str(), Some("p5-ss-detect"));
    assert_eq!(v["limit"].as_i64(), Some(1));
    let matches = v["matches"].as_array().expect("matches");
    assert_eq!(matches.len(), 1, "negative limit clamps to one match");
    let snippet = matches[0]["snippet"].as_str().unwrap_or_default();
    assert!(
        snippet.contains("static mut")
            || snippet.contains("unsafe impl Send")
            || snippet.contains("Arc::<RefCell"),
        "unexpected match snippet: {snippet}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn send_sync_violations_rejects_duplicate_project_names() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-ss-dup", "/ws/p5-ss-dup").await;
    seed_file(db.pool(), p, "/ws/p5-ss-dup/a.rs", "a.rs").await;
    seed_project(db.pool(), "p5-ss-dup", "/ws/p5-ss-dup-shadow").await;
    let server = server_with_pool(db.pool().clone());
    let err = server
        .call_tool_cli(
            "send_sync_violations",
            serde_json::json!({"project": "p5-ss-dup"}),
        )
        .await
        .expect_err("duplicate project display names must fail closed");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("ambiguous project name") || msg.contains("not unique"),
        "error should identify duplicate project name; got {msg}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn quadratic_loops_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-ql", "/ws/p5-ql").await;
    seed_file(db.pool(), p, "/ws/p5-ql/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("quadratic_loops", serde_json::json!({"project": "p5-ql"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_preallocation_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-mp", "/ws/p5-mp").await;
    seed_file(db.pool(), p, "/ws/p5-mp/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "missing_preallocation",
            serde_json::json!({"project": "p5-mp"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn blocking_in_async_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-ba", "/ws/p5-ba").await;
    seed_file(db.pool(), p, "/ws/p5-ba/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("blocking_in_async", serde_json::json!({"project": "p5-ba"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn clone_density_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-cd", "/ws/p5-cd").await;
    seed_file(db.pool(), p, "/ws/p5-cd/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("clone_density", serde_json::json!({"project": "p5-cd"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn io_hotpath_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-ih", "/ws/p5-ih").await;
    seed_file(db.pool(), p, "/ws/p5-ih/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("io_hotpath", serde_json::json!({"project": "p5-ih"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}
