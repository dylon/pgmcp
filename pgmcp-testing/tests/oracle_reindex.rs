//! Focused oracle coverage for `reindex`.

use pgmcp_testing::pool_tool_helpers::{seed_project, server_with_pool};
use pgmcp_testing::require_test_db;
use serde_json::json;
use uuid::Uuid;

async fn seed_file_with_language(
    pool: &sqlx::PgPool,
    project_id: i32,
    path: &str,
    relative_path: &str,
    language: &str,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO indexed_files
            (project_id, path, relative_path, language, size_bytes, content,
             content_hash, line_count, modified_at)
         VALUES ($1, $2, $3, $4, 10, 'content', 1, 1, now())
         RETURNING id",
    )
    .bind(project_id)
    .bind(path)
    .bind(relative_path)
    .bind(language)
    .fetch_one(pool)
    .await
    .expect("seed indexed file")
}

async fn count_files_by_language(pool: &sqlx::PgPool, project_id: i32, language: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM indexed_files WHERE project_id = $1 AND language = $2")
        .bind(project_id)
        .bind(language)
        .fetch_one(pool)
        .await
        .expect("count language files")
}

#[tokio::test(flavor = "multi_thread")]
async fn reindex_language_mode_normalizes_and_deletes_only_that_language() {
    let db = require_test_db!();
    let pool = db.pool();
    let suffix = Uuid::new_v4();
    let project_path = format!("/ws/reindex-{suffix}");
    let project = seed_project(pool, "reindex-language", &project_path).await;

    seed_file_with_language(
        pool,
        project,
        &format!("{project_path}/src/lib.rs"),
        "src/lib.rs",
        "rust",
    )
    .await;
    seed_file_with_language(
        pool,
        project,
        &format!("{project_path}/script.py"),
        "script.py",
        "python",
    )
    .await;

    let server = server_with_pool(pool.clone());
    let result = server
        .call_tool_cli("reindex", json!({"language": " Rust "}))
        .await
        .expect("language reindex");
    assert!(result.is_error != Some(true));

    assert_eq!(count_files_by_language(pool, project, "rust").await, 0);
    assert_eq!(count_files_by_language(pool, project, "python").await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn reindex_rejects_invalid_language_tokens_before_deleting() {
    let db = require_test_db!();
    let pool = db.pool();
    let suffix = Uuid::new_v4();
    let project_path = format!("/ws/reindex-invalid-{suffix}");
    let project = seed_project(pool, "reindex-invalid", &project_path).await;
    seed_file_with_language(
        pool,
        project,
        &format!("{project_path}/src/lib.rs"),
        "src/lib.rs",
        "rust",
    )
    .await;

    let server = server_with_pool(pool.clone());
    for bad_language in ["   ", "rust/../../x", &"x".repeat(65)] {
        assert!(
            server
                .call_tool_cli("reindex", json!({"language": bad_language}))
                .await
                .is_err(),
            "invalid language token must fail closed: {bad_language:?}"
        );
    }

    assert_eq!(count_files_by_language(pool, project, "rust").await, 1);
}
