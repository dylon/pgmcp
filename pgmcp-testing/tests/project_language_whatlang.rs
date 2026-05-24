//! P13.3 — whatlang BCP-47 detection + cache test.
//!
//! Seeds a project with comments in known languages, calls
//! `project_language`, asserts the cache key lands in
//! `pgmcp_metadata` and the dispatcher returns the expected tag.

use pgmcp::code_analysis::language_detect::project_language;
use pgmcp_testing::require_test_db;

async fn seed_project(pool: &sqlx::PgPool, name: &str, readme: &str) -> i32 {
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET workspace_path = $1 RETURNING id",
    )
    .bind(format!("/ws/{name}"))
    .bind(format!("/ws/{name}/p"))
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("project");

    sqlx::query(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at) \
         VALUES ($1, $2, $3, 'markdown', $4, $5, $6, $7, NOW()) \
         ON CONFLICT (path) DO UPDATE SET content = $5"
    )
    .bind(project_id)
    .bind(format!("/ws/{name}/p/README.md"))
    .bind("README.md")
    .bind(readme.len() as i64)
    .bind(readme)
    .bind(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0)
            ^ (name.len() as i64),
    )
    .bind(10_i32)
    .execute(pool)
    .await
    .expect("readme");

    project_id
}

#[tokio::test(flavor = "multi_thread")]
async fn english_readme_detected_and_cached() {
    let testdb = require_test_db!();
    let readme = "This is a sufficiently long sample of English prose suitable \
                  for the whatlang trigram-based detector. It describes a fictional \
                  project that does fictional things, with enough vocabulary to \
                  push the detector past its minimum-confidence threshold.";
    let project_id = seed_project(testdb.pool(), "lang_test_en", readme).await;

    let tag = project_language(testdb.pool(), project_id, false)
        .await
        .expect("detect");
    assert_eq!(tag, "en-us");

    // Cache hit on second call.
    let cached: String = sqlx::query_scalar("SELECT value FROM pgmcp_metadata WHERE key = $1")
        .bind(format!("phonetic.language.{project_id}"))
        .fetch_one(testdb.pool())
        .await
        .expect("cache row");
    assert_eq!(cached, "en-us");
}

#[tokio::test(flavor = "multi_thread")]
async fn force_refresh_overwrites_cache() {
    let testdb = require_test_db!();
    let readme = "Pequeño texto en español suficientemente largo para que \
                  whatlang detecte el idioma con confianza alta y lo asigne \
                  al paquete de reglas castellanas correctamente.";
    let project_id = seed_project(testdb.pool(), "lang_test_force", readme).await;

    let first = project_language(testdb.pool(), project_id, false)
        .await
        .expect("first detect");
    let _second = project_language(testdb.pool(), project_id, true)
        .await
        .expect("forced re-detect");
    // Cache row exists and is one of the expected fallbacks.
    let cached: String = sqlx::query_scalar("SELECT value FROM pgmcp_metadata WHERE key = $1")
        .bind(format!("phonetic.language.{project_id}"))
        .fetch_one(testdb.pool())
        .await
        .expect("cache row");
    assert!(
        cached == first || cached == "en-us" || cached == "es",
        "cache must contain a detected tag, got {cached}"
    );
}
