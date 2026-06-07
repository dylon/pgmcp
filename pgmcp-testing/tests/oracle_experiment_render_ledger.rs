//! Oracle tests for `experiment_render_ledger`.
//!
//! These pin the boundary modeled in
//! `docs/formal/tla/ExperimentRenderLedgerScope.tla`: safe experiment lookup,
//! dry-run no-write behavior, contained ledger paths, safe filenames, atomic
//! write semantics, and no database mutation.

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use common::{server_with_pool, text_of};
use pgmcp::config::Config;
use pgmcp::mcp::server::McpServer;
use pgmcp_testing::pool_tool_helpers::context_with_pool;
use pgmcp_testing::require_test_db;
use serde_json::{Value, json};
use uuid::Uuid;

async fn insert_experiment(pool: &sqlx::PgPool, slug: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO experiments (slug, title, question, kind, status)
         VALUES ($1, $2, $3, 'investigation'::experiment_kind, 'open'::experiment_status)
         RETURNING id",
    )
    .bind(slug)
    .bind(format!("title {slug}"))
    .bind(format!("question {slug}"))
    .fetch_one(pool)
    .await
    .expect("insert experiment")
}

async fn experiment_count(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM experiments")
        .fetch_one(pool)
        .await
        .expect("count experiments")
}

fn server_with_ledger_dir(pool: sqlx::PgPool, ledger_dir: String) -> McpServer {
    let mut cfg = Config::default();
    cfg.experiments.ledger_dir = ledger_dir;
    let ctx = context_with_pool(pool);
    ctx.config().store(Arc::new(cfg));
    McpServer::new(ctx)
}

#[tokio::test]
async fn experiment_render_ledger_dry_run_trims_slug_and_writes_nothing() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let suffix = Uuid::new_v4().simple();
    let slug = format!("render-dry-{suffix}");
    insert_experiment(&pool, &slug).await;
    let before = experiment_count(&pool).await;
    let server = server_with_pool(pool.clone());

    let result = server
        .call_tool_cli(
            "experiment_render_ledger",
            json!({
                "slug": format!(" {slug} "),
                "dry_run": true,
            }),
        )
        .await
        .expect("render ledger dry run");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["written"].as_bool(), Some(false));
    assert!(v["bytes"].as_u64().unwrap_or(0) > 0);
    let content = v["content"].as_str().expect("dry-run content");
    assert!(content.contains(&format!("pgmcp_experiment: {slug}")));
    let path = PathBuf::from(v["path"].as_str().expect("path"));
    assert!(
        !path.exists(),
        "dry run must not create the rendered ledger path"
    );
    assert_eq!(
        experiment_count(&pool).await,
        before,
        "render must not mutate experiment rows"
    );
}

#[tokio::test]
async fn experiment_render_ledger_writes_inside_configured_relative_dir() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let suffix = Uuid::new_v4().simple();
    let slug = format!("render-write-{suffix}");
    let experiment_id = insert_experiment(&pool, &slug).await;
    let ledger_dir = format!("target/pgmcp-ledger-tests/{suffix}");
    let _ = std::fs::remove_dir_all(&ledger_dir);
    let server = server_with_ledger_dir(pool, ledger_dir.clone());

    let result = server
        .call_tool_cli(
            "experiment_render_ledger",
            json!({
                "experiment_id": experiment_id,
                "dry_run": false,
            }),
        )
        .await
        .expect("render ledger write");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["written"].as_bool(), Some(true));
    assert!(
        v["content"].is_null(),
        "write response must not echo content"
    );
    let path = PathBuf::from(v["path"].as_str().expect("path"));
    assert!(path.exists(), "ledger file must be written");
    assert!(
        path.starts_with(std::env::current_dir().unwrap().join(&ledger_dir)),
        "ledger path must stay inside configured relative dir: {path:?}"
    );
    let content = std::fs::read_to_string(&path).expect("read rendered ledger");
    assert!(content.contains(&format!("pgmcp_experiment: {slug}")));
    let temp_leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .expect("read ledger dir")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(
        temp_leftovers.is_empty(),
        "atomic write temp files should not remain after a successful render"
    );
    let _ = std::fs::remove_dir_all(ledger_dir);
}

#[tokio::test]
async fn experiment_render_ledger_rejects_unsafe_paths_and_slugs() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let suffix = Uuid::new_v4().simple();
    let unsafe_slug_id = insert_experiment(&pool, &format!("../render-escape-{suffix}")).await;

    let unsafe_dir_server = server_with_ledger_dir(pool.clone(), "../outside".to_string());
    assert!(
        unsafe_dir_server
            .call_tool_cli(
                "experiment_render_ledger",
                json!({
                    "experiment_id": unsafe_slug_id,
                    "dry_run": true,
                }),
            )
            .await
            .is_err(),
        "parent-directory ledger_dir must reject"
    );

    let safe_dir_server =
        server_with_ledger_dir(pool, format!("target/pgmcp-ledger-tests/reject-{suffix}"));
    assert!(
        safe_dir_server
            .call_tool_cli(
                "experiment_render_ledger",
                json!({
                    "experiment_id": unsafe_slug_id,
                    "dry_run": true,
                }),
            )
            .await
            .is_err(),
        "unsafe stored slugs must not become ledger filenames"
    );
}
