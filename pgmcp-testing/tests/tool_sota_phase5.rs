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

async fn seed_panic_function(
    pool: &sqlx::PgPool,
    project_id: i32,
    file_id: i64,
    name: &str,
    visibility: &str,
    panic_paths: i32,
    cyclomatic: i32,
) -> i64 {
    let symbol_id: i64 = sqlx::query_scalar(
        "INSERT INTO file_symbols (file_id, name, kind, start_line, end_line, visibility)
         VALUES ($1, $2, 'function', 1, 3, $3)
         RETURNING id",
    )
    .bind(file_id)
    .bind(name)
    .bind(visibility)
    .fetch_one(pool)
    .await
    .expect("file symbol");

    sqlx::query(
        "INSERT INTO function_metrics
            (function_id, file_id, project_id, cyclomatic, panic_paths)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(symbol_id)
    .bind(file_id)
    .bind(project_id)
    .bind(cyclomatic)
    .bind(panic_paths)
    .execute(pool)
    .await
    .expect("function metrics");
    symbol_id
}

async fn seed_effect_symbol(
    pool: &sqlx::PgPool,
    file_id: i64,
    name: &str,
    effects: &[&str],
) -> i64 {
    let symbol_id: i64 = sqlx::query_scalar(
        "INSERT INTO file_symbols (file_id, name, kind, start_line, end_line, visibility)
         VALUES ($1, $2, 'function', 1, 3, 'private')
         RETURNING id",
    )
    .bind(file_id)
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("file symbol");

    for effect in effects {
        sqlx::query("INSERT INTO symbol_effects (symbol_id, effect) VALUES ($1, $2)")
            .bind(symbol_id)
            .bind(effect)
            .execute(pool)
            .await
            .expect("symbol effect");
    }

    symbol_id
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
async fn lockset_races_scopes_effect_breakdown_to_project() {
    let db = require_test_db!();
    let suffix = Uuid::now_v7().simple();
    let target = format!("p5-lockset-scope-{suffix}");
    let other = format!("p5-lockset-other-{suffix}");
    let target_id = seed_project(db.pool(), &target, &format!("/ws/{target}")).await;
    let other_id = seed_project(db.pool(), &other, &format!("/ws/{other}")).await;
    let target_file = seed_file(db.pool(), target_id, &format!("/ws/{target}/a.rs"), "a.rs").await;
    let other_file = seed_file(db.pool(), other_id, &format!("/ws/{other}/b.rs"), "b.rs").await;
    sqlx::query("UPDATE indexed_files SET content = $1, language = 'rust' WHERE id = $2")
        .bind("fn guarded(m: std::sync::Mutex<u8>) { let _g = m.lock(); }\n")
        .bind(target_file)
        .execute(db.pool())
        .await
        .expect("seed target content");

    seed_effect_symbol(db.pool(), target_file, "guarded", &["lock_acquire"]).await;
    seed_effect_symbol(db.pool(), other_file, "unrelated", &["may_panic"]).await;

    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "lockset_races",
            serde_json::json!({"project": format!(" {target} "), "limit": 5}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
    let v: serde_json::Value = serde_json::from_str(&text_of(&r)).expect("lockset JSON");
    assert_eq!(v["project"].as_str(), Some(target.as_str()));
    assert_eq!(v["limit"].as_u64(), Some(5));
    let effects = v["effect_breakdown"]
        .as_object()
        .expect("effect_breakdown object map");
    assert!(
        effects.contains_key("lock_acquire"),
        "target effect missing: {effects:?}"
    );
    assert!(
        !effects.contains_key("may_panic"),
        "other project effect leaked into lockset_races: {effects:?}"
    );
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
    let name = format!("p5-pp-{}", Uuid::now_v7().simple());
    let root = format!("/ws/{name}");
    let p = seed_project(db.pool(), &name, &root).await;
    let file_id = seed_file(db.pool(), p, &format!("{root}/a.rs"), "a.rs").await;
    seed_panic_function(db.pool(), p, file_id, "may_panic", "public", 3, 5).await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "panic_paths",
            serde_json::json!({"project": format!(" {name} "), "entry_filter": " pub ", "limit": 0}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
    let v: serde_json::Value = serde_json::from_str(&text_of(&r)).expect("panic JSON");
    assert_eq!(v["project"].as_str(), Some(name.as_str()));
    assert_eq!(v["entry_filter"].as_str(), Some("pub"));
    assert_eq!(v["limit"].as_u64(), Some(1));
    let functions = v["functions"].as_array().expect("functions");
    assert_eq!(functions.len(), 1);
    assert_eq!(functions[0]["file"].as_str(), Some("a.rs"));
    assert_eq!(functions[0]["function"].as_str(), Some("may_panic"));
    assert_eq!(functions[0]["panic_paths"].as_u64(), Some(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn panic_paths_rejects_invalid_entry_filter() {
    let db = require_test_db!();
    let name = format!("p5-pp-filter-{}", Uuid::now_v7().simple());
    let root = format!("/ws/{name}");
    seed_project(db.pool(), &name, &root).await;
    let server = server_with_pool(db.pool().clone());
    let err = server
        .call_tool_cli(
            "panic_paths",
            serde_json::json!({"project": name, "entry_filter": "public"}),
        )
        .await
        .expect_err("invalid entry_filter must fail closed");
    assert!(
        err.to_string().contains("entry_filter must be one of"),
        "unexpected panic_paths entry_filter error: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn panic_paths_rejects_stale_cross_project_metrics() {
    let db = require_test_db!();
    let suffix = Uuid::now_v7().simple().to_string();
    let target_name = format!("p5-pp-target-{suffix}");
    let other_name = format!("p5-pp-other-{suffix}");
    let target_root = format!("/ws/{target_name}");
    let other_root = format!("/ws/{other_name}");
    let target_project = seed_project(db.pool(), &target_name, &target_root).await;
    let other_project = seed_project(db.pool(), &other_name, &other_root).await;
    seed_file(
        db.pool(),
        target_project,
        &format!("{target_root}/target.rs"),
        "target.rs",
    )
    .await;
    let other_file = seed_file(
        db.pool(),
        other_project,
        &format!("{other_root}/other.rs"),
        "other.rs",
    )
    .await;
    seed_panic_function(
        db.pool(),
        target_project,
        other_file,
        "foreign_panic",
        "public",
        9,
        11,
    )
    .await;

    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "panic_paths",
            serde_json::json!({"project": target_name, "entry_filter": "pub"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
    let v: serde_json::Value = serde_json::from_str(&text_of(&r)).expect("panic JSON");
    assert_eq!(
        v["functions"].as_array().expect("functions").len(),
        0,
        "stale function_metrics row must not leak another project's file: {v}"
    );
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
async fn blocking_in_async_streams_and_clamps_limit() {
    let db = require_test_db!();
    let suffix = Uuid::now_v7().simple();
    let project = format!("p5-ba-bound-{suffix}");
    let p = seed_project(db.pool(), &project, &format!("/ws/{project}")).await;
    let file_id = seed_file(db.pool(), p, &format!("/ws/{project}/a.rs"), "a.rs").await;
    sqlx::query("UPDATE indexed_files SET content = $1, language = 'rust' WHERE id = $2")
        .bind(
            "async fn blocked() {\n    std::thread::sleep(std::time::Duration::from_millis(1));\n    std::fs::read_to_string(\"x\").ok();\n}\n",
        )
        .bind(file_id)
        .execute(db.pool())
        .await
        .expect("seed blocking async content");

    seed_effect_symbol(db.pool(), file_id, "blocked", &["async", "blocking_io"]).await;

    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "blocking_in_async",
            serde_json::json!({"project": format!(" {project} "), "limit": -50}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
    let v: serde_json::Value = serde_json::from_str(&text_of(&r)).expect("blocking JSON");
    assert_eq!(v["project"].as_str(), Some(project.as_str()));
    assert_eq!(v["limit"].as_u64(), Some(1));
    let regex_matches = v["regex_matches"].as_array().expect("regex_matches");
    assert_eq!(regex_matches.len(), 1);
    assert_eq!(regex_matches[0]["file"].as_str(), Some("a.rs"));
    let effect_intersection = v["effect_intersection"]
        .as_array()
        .expect("effect_intersection");
    assert_eq!(effect_intersection.len(), 1);
    assert_eq!(effect_intersection[0]["name"].as_str(), Some("blocked"));
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
    let suffix = Uuid::now_v7().simple();
    let project = format!("p5-ih-{suffix}");
    let p = seed_project(db.pool(), &project, &format!("/ws/{project}")).await;
    let a = seed_file(db.pool(), p, &format!("/ws/{project}/a.rs"), "a.rs").await;
    sqlx::query("UPDATE indexed_files SET content = $1, line_count = 1 WHERE id = $2")
        .bind("fn read() { let _ = std::fs::read_to_string(\"x\"); }")
        .bind(a)
        .execute(db.pool())
        .await
        .expect("seed io content");
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "io_hotpath",
            serde_json::json!({"project": format!(" {project} "), "limit": -10}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
    let v: serde_json::Value = serde_json::from_str(&text_of(&r)).expect("io_hotpath JSON");
    assert_eq!(v["project"].as_str(), Some(project.as_str()));
    assert_eq!(v["limit"].as_u64(), Some(1));
    assert_eq!(v["files"].as_array().expect("files").len(), 1);
    assert_eq!(v["files"][0]["file"].as_str(), Some("a.rs"));
}

#[tokio::test(flavor = "multi_thread")]
async fn io_hotpath_rejects_stale_cross_project_metrics() {
    let db = require_test_db!();
    let suffix = Uuid::now_v7().simple();
    let project = format!("p5-ih-scope-{suffix}");
    let other = format!("p5-ih-scope-other-{suffix}");
    let p = seed_project(db.pool(), &project, &format!("/ws/{project}")).await;
    let q = seed_project(db.pool(), &other, &format!("/ws/{other}")).await;
    let hot = seed_file(db.pool(), p, &format!("/ws/{project}/src/a.rs"), "src/a.rs").await;
    let stale = seed_file(db.pool(), q, &format!("/ws/{other}/src/a.rs"), "src/a.rs").await;
    sqlx::query("UPDATE indexed_files SET content = $1, line_count = 1 WHERE id = $2")
        .bind("fn read() { let _ = std::fs::read_to_string(\"x\"); }")
        .bind(hot)
        .execute(db.pool())
        .await
        .expect("seed io content");
    sqlx::query(
        "INSERT INTO file_metrics (file_id, project_id, pagerank, betweenness)
         VALUES ($1, $2, 0.9, 0.8)",
    )
    .bind(stale)
    .bind(p)
    .execute(db.pool())
    .await
    .expect("stale file_metrics");

    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "io_hotpath",
            serde_json::json!({"project": project, "limit": 10}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
    let v: serde_json::Value = serde_json::from_str(&text_of(&r)).expect("io_hotpath JSON");
    let files = v["files"].as_array().expect("files");

    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["file"].as_str(), Some("src/a.rs"));
    assert_eq!(
        files[0]["pagerank"].as_f64(),
        Some(0.0),
        "stale metrics from another project's file must not weight this project's hit: {v}"
    );
}
