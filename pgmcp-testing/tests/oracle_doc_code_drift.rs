//! Oracle tests for `doc_code_drift`.
//!
//! These tests pin the formal boundary modeled in
//! `docs/formal/tla/DocCodeDriftScope.tla`: normalize parameters before any
//! read, resolve a unique project once, scope every read/enrichment channel to
//! that resolved id, bound returned rows at SQL level, and keep execution
//! read-only.

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::pool_tool_helpers::{seed_file_symbol, seed_project};
use pgmcp_testing::require_test_db;
use serde_json::Value;
use uuid::Uuid;

const D: usize = 1024;

fn basis(axis: usize) -> Vec<f32> {
    let mut v = vec![0.0; D];
    v[axis] = 1.0;
    v
}

async fn insert_file(
    pool: &sqlx::PgPool,
    project_id: i32,
    workspace: &str,
    relative_path: &str,
    language: &str,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO indexed_files
            (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at)
         VALUES ($1, $2, $3, $4, 64, 'synthetic', 1, 1, NOW())
         RETURNING id",
    )
    .bind(project_id)
    .bind(format!("{workspace}/{relative_path}"))
    .bind(relative_path)
    .bind(language)
    .fetch_one(pool)
    .await
    .expect("insert indexed file")
}

async fn insert_chunk(pool: &sqlx::PgPool, file_id: i64, chunk_index: i32, axis: usize) {
    sqlx::query(
        "INSERT INTO file_chunks
            (file_id, chunk_index, content, start_line, end_line, embedding_v2, embedding_signature)
         VALUES ($1, $2, 'chunk', 1, 1, $3, 'bge-m3-v1')",
    )
    .bind(file_id)
    .bind(chunk_index)
    .bind(pgvector::Vector::from(basis(axis)))
    .execute(pool)
    .await
    .expect("insert file chunk");
}

async fn seed_doc_code_dir(
    pool: &sqlx::PgPool,
    project_id: i32,
    workspace: &str,
    dir: &str,
    doc_axis: usize,
    code_axis: usize,
) -> i64 {
    let doc = insert_file(
        pool,
        project_id,
        workspace,
        &format!("{dir}/readme.md"),
        "markdown",
    )
    .await;
    let code = insert_file(
        pool,
        project_id,
        workspace,
        &format!("{dir}/lib.rs"),
        "rust",
    )
    .await;
    insert_chunk(pool, doc, 0, doc_axis).await;
    insert_chunk(pool, code, 0, code_axis).await;
    code
}

async fn insert_effect(pool: &sqlx::PgPool, file_id: i64, name: &str, effect: &str) {
    let symbol_id = seed_file_symbol(pool, file_id, name, "function", 1, None).await;
    sqlx::query("INSERT INTO symbol_effects (symbol_id, effect) VALUES ($1, $2)")
        .bind(symbol_id)
        .bind(effect)
        .execute(pool)
        .await
        .expect("insert symbol effect");
}

async fn row_counts(pool: &sqlx::PgPool) -> (i64, i64, i64, i64) {
    let projects = sqlx::query_scalar("SELECT COUNT(*) FROM projects")
        .fetch_one(pool)
        .await
        .expect("count projects");
    let files = sqlx::query_scalar("SELECT COUNT(*) FROM indexed_files")
        .fetch_one(pool)
        .await
        .expect("count files");
    let chunks = sqlx::query_scalar("SELECT COUNT(*) FROM file_chunks")
        .fetch_one(pool)
        .await
        .expect("count chunks");
    let effects = sqlx::query_scalar("SELECT COUNT(*) FROM symbol_effects")
        .fetch_one(pool)
        .await
        .expect("count effects");
    (projects, files, chunks, effects)
}

#[tokio::test]
async fn doc_code_drift_trims_project_scopes_effects_and_is_read_only() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let suffix = Uuid::new_v4().simple();
    let target_name = format!("dcd-target-{suffix}");
    let other_name = format!("dcd-other-{suffix}");
    let target_workspace = format!("/ws/{target_name}");
    let other_workspace = format!("/ws/{other_name}");
    let target_project = seed_project(&pool, &target_name, &target_workspace).await;
    let other_project = seed_project(&pool, &other_name, &other_workspace).await;
    let target_code =
        seed_doc_code_dir(&pool, target_project, &target_workspace, "docs", 0, 1).await;
    let other_code = seed_doc_code_dir(&pool, other_project, &other_workspace, "docs", 0, 1).await;
    insert_effect(
        &pool,
        target_code,
        "target_async",
        pgmcp::parsing::type_tags::vocabulary::EFFECT_ASYNC,
    )
    .await;
    insert_effect(
        &pool,
        other_code,
        "other_unsafe",
        pgmcp::parsing::type_tags::vocabulary::EFFECT_UNSAFE,
    )
    .await;
    let before = row_counts(&pool).await;

    let server = server_with_pool(pool.clone());
    let result = server
        .call_tool_cli(
            "doc_code_drift",
            serde_json::json!({
                "project": format!(" {target_name} "),
                "min_drift": 0.5,
                "limit": 5,
            }),
        )
        .await
        .expect("doc_code_drift call");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["project"].as_str(), Some(target_name.as_str()));
    assert_eq!(v["limit"].as_i64(), Some(5));
    assert_eq!(v["directories"].as_array().unwrap().len(), 1);
    assert_eq!(v["directories"][0]["directory"].as_str(), Some("docs"));

    let effects: std::collections::BTreeMap<&str, i64> = v["effect_breakdown"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(effect, count)| (effect.as_str(), count.as_i64().unwrap()))
        .collect();
    assert_eq!(
        effects.get(pgmcp::parsing::type_tags::vocabulary::EFFECT_ASYNC),
        Some(&1)
    );
    assert!(
        !effects.contains_key(pgmcp::parsing::type_tags::vocabulary::EFFECT_UNSAFE),
        "effect enrichment must not include other projects"
    );
    assert_eq!(row_counts(&pool).await, before, "tool must be read-only");
}

#[tokio::test]
async fn doc_code_drift_rejects_duplicate_project_names() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let suffix = Uuid::new_v4().simple();
    let project_name = format!("dcd-dup-{suffix}");
    seed_project(&pool, &project_name, &format!("/ws/{project_name}-a")).await;
    seed_project(&pool, &project_name, &format!("/ws/{project_name}-b")).await;
    let server = server_with_pool(pool);

    assert!(
        server
            .call_tool_cli(
                "doc_code_drift",
                serde_json::json!({
                    "project": project_name,
                    "min_drift": 0.0,
                }),
            )
            .await
            .is_err(),
        "duplicate project names must fail closed"
    );
}

#[tokio::test]
async fn doc_code_drift_clamps_threshold_and_limit() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let suffix = Uuid::new_v4().simple();
    let project_name = format!("dcd-clamp-{suffix}");
    let workspace = format!("/ws/{project_name}");
    let project_id = seed_project(&pool, &project_name, &workspace).await;
    seed_doc_code_dir(&pool, project_id, &workspace, "docs", 0, 1).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "doc_code_drift",
            serde_json::json!({
                "project": project_name,
                "min_drift": -5.0,
                "limit": -10,
            }),
        )
        .await
        .expect("doc_code_drift call");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["min_drift"].as_f64(), Some(0.0));
    assert_eq!(v["limit"].as_i64(), Some(0));
    assert!(
        v["directories"].as_array().unwrap().is_empty(),
        "zero limit must return no directories"
    );
}
