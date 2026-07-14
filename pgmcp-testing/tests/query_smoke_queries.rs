//! Layer B of the integration-test plan: one execution test per public
//! function in `src/db/queries.rs` (~106 functions).
//!
//! Each test:
//!   1. opens a `TestDatabase` via `require_test_db!()`
//!   2. seeds the synthetic corpus (`SyntheticCorpus::seed_with_assignments`)
//!   3. calls the function with reasonable args
//!   4. asserts the future resolves to `Ok(_)`
//!
//! Rationale: the orient bug (commit 802ca00) shipped because the
//! tool's inline SQL was never executed against a populated derived
//! table. The same risk exists for every `pub async fn` in
//! `queries.rs`. Driving each function once against the real schema
//! catches column-name drift, type drift, and query-syntax bugs at
//! `cargo test` time rather than in production.
//!
//! The tests are intentionally minimal: they don't assert algorithmic
//! correctness (that's what `oracle_*.rs` files cover). They prove
//! that the SQL parses, the bindings match, and the result type
//! decodes.

use chrono::Utc;
use pgmcp::db::queries;
use pgmcp_testing::fixtures::synthetic_corpus::SyntheticCorpus;
use pgmcp_testing::require_test_db;
use sqlx::PgPool;
use uuid::Uuid;

/// Pull any file_id out of indexed_files. The synthetic corpus seeds
/// at least 6 files so this never returns None.
async fn any_file_id(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT id FROM indexed_files LIMIT 1")
        .fetch_one(pool)
        .await
        .expect("any_file_id")
}

/// Pull any indexed-file `path` out of the seeded corpus.
async fn any_file_path(pool: &PgPool) -> String {
    sqlx::query_scalar("SELECT path FROM indexed_files LIMIT 1")
        .fetch_one(pool)
        .await
        .expect("any_file_path")
}

/// Pull any indexed-file `relative_path` out of the seeded corpus.
async fn any_relative_path(pool: &PgPool) -> String {
    sqlx::query_scalar("SELECT relative_path FROM indexed_files LIMIT 1")
        .fetch_one(pool)
        .await
        .expect("any_relative_path")
}

/// Test L2-normalised embedding the seeder uses (basis 0).
fn test_embedding() -> Vec<f32> {
    pgmcp_testing::fixtures::synthetic_corpus::basis(0)
}

// =============================================================================
// Project CRUD + metadata (12 functions)
// =============================================================================

#[tokio::test]
async fn queries_upsert_project_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::upsert_project(
        db.pool(),
        "/ws-other",
        "/ws-other/proj-x",
        "proj-x",
        None,
        None,
    )
    .await
    .expect("upsert_project");
}

#[tokio::test]
async fn queries_list_projects_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::list_projects(db.pool())
        .await
        .expect("list_projects");
}

#[tokio::test]
async fn queries_list_projects_preserves_duplicate_display_names() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;

    let project_a = queries::upsert_project(
        db.pool(),
        "/ws/list-ambiguous-a",
        "/ws/list-ambiguous-a/shared",
        "duplicate-list-name",
        None,
        None,
    )
    .await
    .expect("insert first duplicate-name project");
    let project_b = queries::upsert_project(
        db.pool(),
        "/ws/list-ambiguous-b",
        "/ws/list-ambiguous-b/shared",
        "duplicate-list-name",
        None,
        None,
    )
    .await
    .expect("insert second duplicate-name project");

    let projects = queries::list_projects(db.pool())
        .await
        .expect("list_projects");
    let duplicate_rows: Vec<_> = projects
        .iter()
        .filter(|p| p.name == "duplicate-list-name")
        .collect();
    let ids: std::collections::HashSet<_> = duplicate_rows.iter().map(|p| p.id).collect();
    let paths: std::collections::HashSet<_> =
        duplicate_rows.iter().map(|p| p.path.as_str()).collect();

    assert_eq!(
        duplicate_rows.len(),
        2,
        "list_projects must enumerate duplicate display names as distinct project rows"
    );
    assert_eq!(
        ids.len(),
        2,
        "duplicate display-name rows must keep distinct ids"
    );
    assert!(ids.contains(&project_a));
    assert!(ids.contains(&project_b));
    assert!(paths.contains("/ws/list-ambiguous-a/shared"));
    assert!(paths.contains("/ws/list-ambiguous-b/shared"));
}

#[tokio::test]
async fn queries_find_project_by_cwd_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::find_project_by_cwd(db.pool(), "/ws/auth/proj-auth")
        .await
        .expect("find_project_by_cwd");
}

#[tokio::test]
async fn queries_find_project_by_cwd_respects_path_boundary() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;

    queries::upsert_project(
        db.pool(),
        "/ws/boundary",
        "/ws/boundary/app",
        "boundary-app",
        None,
        None,
    )
    .await
    .expect("insert boundary project");

    let exact_child = queries::find_project_by_cwd(db.pool(), "/ws/boundary/app/src/lib.rs")
        .await
        .expect("find exact-child project")
        .expect("exact child should resolve");
    assert_eq!(exact_child.name, "boundary-app");

    let sibling_prefix = queries::find_project_by_cwd(db.pool(), "/ws/boundary/application/src")
        .await
        .expect("find sibling-prefix project");
    assert!(
        sibling_prefix.is_none(),
        "project paths must match only exact paths or directory boundaries"
    );
}

#[tokio::test]
async fn queries_select_main_worktree_projects_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::select_main_worktree_projects(db.pool())
        .await
        .expect("select_main_worktree_projects");
}

#[tokio::test]
async fn queries_language_summary_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::language_summary(db.pool(), "proj-auth")
        .await
        .expect("language_summary");
}

#[tokio::test]
async fn queries_update_project_scanned_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    queries::update_project_scanned(db.pool(), h.auth_project_id)
        .await
        .expect("update_project_scanned");
}

#[tokio::test]
async fn queries_get_all_file_metadata_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::get_all_file_metadata(db.pool())
        .await
        .expect("get_all_file_metadata");
}

#[tokio::test]
async fn queries_count_indexed_files_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::count_indexed_files(db.pool())
        .await
        .expect("count_indexed_files");
}

#[tokio::test]
async fn queries_count_chunks_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::count_chunks(db.pool())
        .await
        .expect("count_chunks");
}

#[tokio::test]
async fn queries_count_projects_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::count_projects(db.pool())
        .await
        .expect("count_projects");
}

#[tokio::test]
async fn queries_total_bytes_indexed_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::total_bytes_indexed(db.pool())
        .await
        .expect("total_bytes_indexed");
}

#[tokio::test]
async fn queries_list_project_names_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::list_project_names(db.pool())
        .await
        .expect("list_project_names");
}

// =============================================================================
// File CRUD + metadata (12 functions)
// =============================================================================

#[tokio::test]
async fn queries_upsert_file_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::upsert_file(
        db.pool(),
        h.auth_project_id,
        "/ws/auth/new-file.rs",
        "new-file.rs",
        "rust",
        42,
        Some("fn x() {}"),
        Some(0xdead_beefi64),
        1,
        false,
        true,
        Utc::now(),
    )
    .await
    .expect("upsert_file");
}

#[tokio::test]
async fn queries_get_content_hash_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let path = any_file_path(db.pool()).await;
    let _ = queries::get_content_hash(db.pool(), &path)
        .await
        .expect("get_content_hash");
}

#[tokio::test]
async fn queries_finalize_file_hash_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let id = any_file_id(db.pool()).await;
    queries::finalize_file_hash(db.pool(), id, 0xabci64)
        .await
        .expect("finalize_file_hash");
}

#[tokio::test]
async fn queries_delete_file_chunks_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    // Pick a file we don't care about preserving (the split candidate).
    let id = any_file_id(db.pool()).await;
    queries::delete_file_chunks(db.pool(), id)
        .await
        .expect("delete_file_chunks");
}

#[tokio::test]
async fn queries_insert_chunk_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let id = any_file_id(db.pool()).await;
    let emb = test_embedding();
    queries::insert_chunk(db.pool(), id, 999, "test", 1, 2, &emb)
        .await
        .expect("insert_chunk");
}

#[tokio::test]
async fn queries_delete_file_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    queries::delete_file(db.pool(), "/path/that/does/not/exist")
        .await
        .expect("delete_file");
}

#[tokio::test]
async fn queries_delete_files_batch_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::delete_files_batch(db.pool(), &["/nonexistent".into()])
        .await
        .expect("delete_files_batch");
}

#[tokio::test]
async fn queries_read_file_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let path = any_file_path(db.pool()).await;
    let _ = queries::read_file(db.pool(), &path)
        .await
        .expect("read_file");
}

#[tokio::test]
async fn queries_read_file_is_exact_absolute_path_only() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;

    sqlx::query(
        "INSERT INTO indexed_files
            (project_id, path, relative_path, language, size_bytes, content,
             content_hash, line_count, truncated, content_recoverable_from_disk,
             modified_at)
         VALUES
            ($1, '/ws/database/auth/file_0.rs', 'auth/file_0.rs', 'rust', 64,
             'database project duplicate relative path', 777, 1, false, false, $2)",
    )
    .bind(h.database_project_id)
    .bind(Utc::now())
    .execute(db.pool())
    .await
    .expect("insert duplicate relative path in another project");

    let auth_file = queries::read_file(db.pool(), "/ws/auth/auth/file_0.rs")
        .await
        .expect("read auth file")
        .expect("auth file exists");
    assert_eq!(auth_file.path, "/ws/auth/auth/file_0.rs");
    assert_eq!(auth_file.relative_path, "auth/file_0.rs");
    assert_eq!(auth_file.content.as_deref(), Some("synthetic"));

    let database_file = queries::read_file(db.pool(), "/ws/database/auth/file_0.rs")
        .await
        .expect("read database duplicate")
        .expect("database duplicate exists");
    assert_eq!(database_file.path, "/ws/database/auth/file_0.rs");
    assert_eq!(
        database_file.content.as_deref(),
        Some("database project duplicate relative path")
    );

    let relative_lookup = queries::read_file(db.pool(), "auth/file_0.rs")
        .await
        .expect("relative-looking path query must execute");
    assert!(
        relative_lookup.is_none(),
        "read_file must not reinterpret a relative path as an absolute file"
    );
}

#[tokio::test]
async fn queries_read_file_by_relative_path_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let rel = any_relative_path(db.pool()).await;
    let _ = queries::read_file_by_relative_path(db.pool(), &rel)
        .await
        .expect("read_file_by_relative_path");
}

#[tokio::test]
async fn queries_file_info_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let path = any_file_path(db.pool()).await;
    let _ = queries::file_info(db.pool(), &path)
        .await
        .expect("file_info");
}

#[tokio::test]
async fn queries_file_info_reports_project_name() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;

    let info = queries::file_info(db.pool(), "/ws/auth/auth/file_0.rs")
        .await
        .expect("file_info")
        .expect("auth fixture file");

    assert_eq!(info.project_name.as_deref(), Some("proj-auth"));
}

#[tokio::test]
async fn queries_project_tree_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::project_tree(db.pool(), "proj-auth", 3)
        .await
        .expect("project_tree");
}

#[tokio::test]
async fn queries_project_tree_rejects_ambiguous_project_name() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;

    let project_a = queries::upsert_project(
        db.pool(),
        "/ws/ambiguous-a",
        "/ws/ambiguous-a/shared",
        "duplicate-display-name",
        None,
        None,
    )
    .await
    .expect("insert first duplicate-name project");
    let project_b = queries::upsert_project(
        db.pool(),
        "/ws/ambiguous-b",
        "/ws/ambiguous-b/shared",
        "duplicate-display-name",
        None,
        None,
    )
    .await
    .expect("insert second duplicate-name project");

    queries::upsert_file(
        db.pool(),
        project_a,
        "/ws/ambiguous-a/shared/a.rs",
        "a.rs",
        "rust",
        1,
        Some("fn a() {}"),
        Some(1),
        1,
        false,
        false,
        Utc::now(),
    )
    .await
    .expect("insert first duplicate-name file");
    queries::upsert_file(
        db.pool(),
        project_b,
        "/ws/ambiguous-b/shared/b.rs",
        "b.rs",
        "rust",
        1,
        Some("fn b() {}"),
        Some(2),
        1,
        false,
        false,
        Utc::now(),
    )
    .await
    .expect("insert second duplicate-name file");

    let err = queries::project_tree(db.pool(), "duplicate-display-name", 3)
        .await
        .expect_err("duplicate project display names must fail closed");

    assert!(
        err.to_string().contains("ambiguous project name"),
        "unexpected project_tree ambiguity error: {err}"
    );
}

#[tokio::test]
async fn queries_list_languages_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::list_languages(db.pool())
        .await
        .expect("list_languages");
}

#[tokio::test]
async fn queries_search_file_paths_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::search_file_paths(db.pool(), "/ws/", 10)
        .await
        .expect("search_file_paths");
}

// =============================================================================
// Search — semantic / text / grep (5 functions)
// =============================================================================

#[tokio::test]
async fn queries_semantic_search_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let emb = test_embedding();
    let _ = queries::semantic_search(db.pool(), &emb, 5, None, None, 100, true)
        .await
        .expect("semantic_search");
}

#[tokio::test]
async fn queries_hybrid_search_chunks_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;

    let emb = test_embedding();
    // 2-leg branch (query_sparse=None) — the exact path `/api/search` takes,
    // which regressed to HTTP 500 when the fused RRF column decoded as NUMERIC
    // into `Option<f64>`. `.expect` fails on that decode error; `score.is_some()`
    // confirms the `::float8` cast lets the fused RRF column decode cleanly.
    let results =
        queries::hybrid_search_chunks(db.pool(), "fn", &emb, 5, 20, None, None, 100, None)
            .await
            .expect("hybrid_search_chunks smoke (2-leg RRF must decode as float8)");
    assert!(
        !results.is_empty(),
        "expected at least one fused hit for seeded corpus"
    );
    assert!(
        results.iter().all(|r| r.score.is_some()),
        "every fused RRF score must decode into Option<f64>"
    );
    assert!(
        results.iter().all(|r| r.chunk_id.is_some()),
        "hybrid_search_chunks must surface chunk_id"
    );
}

#[tokio::test]
async fn queries_text_search_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::text_search(db.pool(), "auth", 5, None, None, true)
        .await
        .expect("text_search");
}

#[tokio::test]
async fn queries_text_search_filters_project() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let results = queries::text_search(db.pool(), "auth", 10, None, Some("proj-auth"), true)
        .await
        .expect("text_search project filter");

    assert!(!results.is_empty(), "expected auth hits in proj-auth");
    assert!(
        results.iter().all(|r| r.path.starts_with("/ws/auth/")),
        "project filter must exclude cross-project FTS hits: {results:?}"
    );
}

#[tokio::test]
async fn queries_text_search_without_project_keeps_unscoped_files() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;

    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files
            (path, relative_path, language, size_bytes, content, content_hash,
             line_count, truncated, content_recoverable_from_disk, modified_at)
         VALUES
            ('/unscoped/no-project.rs', 'no-project.rs', 'rust', 32,
             'fn unscoped_auth_marker() {}', 12345, 1, false, false, $1)
         RETURNING id",
    )
    .bind(Utc::now())
    .fetch_one(db.pool())
    .await
    .expect("insert NULL-project indexed file");

    let emb = test_embedding();
    queries::insert_chunk(
        db.pool(),
        file_id,
        0,
        "fn unscoped_auth_marker() {}",
        1,
        1,
        &emb,
    )
    .await
    .expect("insert NULL-project chunk");

    let unscoped = queries::text_search(db.pool(), "unscoped_auth_marker", 10, None, None, false)
        .await
        .expect("text_search without project filter");
    assert!(
        unscoped.iter().any(|r| r.path == "/unscoped/no-project.rs"),
        "unscoped text_search must retain indexed files with NULL project_id: {unscoped:?}"
    );

    let scoped = queries::text_search(
        db.pool(),
        "unscoped_auth_marker",
        10,
        None,
        Some("proj-auth"),
        false,
    )
    .await
    .expect("text_search with project filter");
    assert!(
        scoped.iter().all(|r| r.path != "/unscoped/no-project.rs"),
        "project-scoped text_search must exclude NULL-project files: {scoped:?}"
    );
}

#[tokio::test]
async fn queries_grep_search_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::grep_search(db.pool(), "fn\\s+", None, 5, true)
        .await
        .expect("grep_search");
}

#[tokio::test]
async fn queries_grep_search_chunks_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::grep_search_chunks(db.pool(), "auth", None, None, None, false, 5, true)
        .await
        .expect("grep_search_chunks");
}

#[tokio::test]
async fn queries_grep_search_chunks_filters_project() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;

    let results = queries::grep_search_chunks(
        db.pool(),
        "auth",
        Some("proj-auth"),
        None,
        None,
        false,
        10,
        true,
    )
    .await
    .expect("grep_search_chunks project filter");

    assert!(!results.is_empty(), "expected auth hits in proj-auth");
    assert!(
        results.iter().all(|r| r.project_name == "proj-auth"),
        "project filter must exclude cross-project grep hits: {results:?}"
    );
}

#[tokio::test]
async fn queries_find_files_by_path_pattern_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::find_files_by_path_pattern(db.pool(), "proj-auth", "%.rs")
        .await
        .expect("find_files_by_path_pattern");
}

// =============================================================================
// Git history + blame (9 functions)
// =============================================================================

#[tokio::test]
async fn queries_upsert_git_commit_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::upsert_git_commit(
        db.pool(),
        h.auth_project_id,
        "deadbeef",
        "dev@example.com",
        Utc::now(),
        "test commit",
        None,
    )
    .await
    .expect("upsert_git_commit");
}

#[tokio::test]
async fn queries_insert_git_commit_chunk_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let commit_id = queries::upsert_git_commit(
        db.pool(),
        h.auth_project_id,
        "feedface",
        "dev@example.com",
        Utc::now(),
        "msg",
        None,
    )
    .await
    .expect("commit");
    let emb = test_embedding();
    queries::insert_git_commit_chunk(db.pool(), commit_id, 0, "diff", &emb)
        .await
        .expect("insert_git_commit_chunk");
}

#[tokio::test]
async fn queries_get_git_last_commit_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::get_git_last_commit(db.pool(), h.auth_project_id)
        .await
        .expect("get_git_last_commit");
}

#[tokio::test]
async fn queries_set_git_last_commit_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    queries::set_git_last_commit(db.pool(), h.auth_project_id, "abc123")
        .await
        .expect("set_git_last_commit");
}

#[tokio::test]
async fn queries_update_blame_for_file_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let id = any_file_id(db.pool()).await;
    queries::update_blame_for_file(db.pool(), id, "sha1", "author", Utc::now(), 1, 10)
        .await
        .expect("update_blame_for_file");
}

#[tokio::test]
async fn queries_semantic_search_commits_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let emb = test_embedding();
    let _ = queries::semantic_search_commits(db.pool(), &emb, 5, None, 100)
        .await
        .expect("semantic_search_commits");
}

#[tokio::test]
async fn queries_get_git_enabled_projects_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::get_git_enabled_projects(db.pool())
        .await
        .expect("get_git_enabled_projects");
}

#[tokio::test]
async fn queries_insert_commit_file_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let commit_id = queries::upsert_git_commit(
        db.pool(),
        h.auth_project_id,
        "abc",
        "a",
        Utc::now(),
        "m",
        None,
    )
    .await
    .expect("commit");
    queries::insert_commit_file(db.pool(), commit_id, "src/x.rs", 'M')
        .await
        .expect("insert_commit_file");
}

#[tokio::test]
async fn queries_get_commits_missing_files_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::get_commits_missing_files(db.pool(), h.auth_project_id)
        .await
        .expect("get_commits_missing_files");
}

#[tokio::test]
async fn queries_has_commit_files_for_project_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::has_commit_files_for_project(db.pool(), "proj-auth")
        .await
        .expect("has_commit_files_for_project");
}

#[tokio::test]
async fn queries_get_file_id_by_path_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let path = any_file_path(db.pool()).await;
    let _ = queries::get_file_id_by_path(db.pool(), &path)
        .await
        .expect("get_file_id_by_path");
}

// =============================================================================
// Similarity (cross-project) (10 functions)
// =============================================================================

#[tokio::test]
async fn queries_batch_find_cross_project_neighbors_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::batch_find_cross_project_neighbors(db.pool(), 0, 50, 5, 0.8, 100)
        .await
        .expect("batch_find_cross_project_neighbors");
}

#[tokio::test]
async fn queries_insert_similarity_pairs_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    // Empty slice short-circuits but still exercises the entry path.
    let _ = queries::insert_similarity_pairs(db.pool(), &[])
        .await
        .expect("insert_similarity_pairs");
}

#[tokio::test]
async fn queries_clear_similarity_table_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    queries::clear_similarity_table(db.pool())
        .await
        .expect("clear_similarity_table");
}

#[tokio::test]
async fn queries_count_similarity_pairs_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::count_similarity_pairs(db.pool())
        .await
        .expect("count_similarity_pairs");
}

#[tokio::test]
async fn queries_top_similar_file_pairs_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::top_similar_file_pairs(db.pool(), 5)
        .await
        .expect("top_similar_file_pairs");
}

#[tokio::test]
async fn queries_max_chunk_id_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::max_chunk_id(db.pool())
        .await
        .expect("max_chunk_id");
}

#[tokio::test]
async fn queries_find_similar_files_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let id = any_file_id(db.pool()).await;
    let _ = queries::find_similar_files(db.pool(), id, 0.5, 5, None, false)
        .await
        .expect("find_similar_files");
}

#[tokio::test]
async fn queries_find_duplicate_file_pairs_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::find_duplicate_file_pairs(db.pool(), 0.9, None, 5, false)
        .await
        .expect("find_duplicate_file_pairs");
}

#[tokio::test]
async fn queries_find_chunk_similarity_pairs_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::find_chunk_similarity_pairs(db.pool(), 0.8, None, &[], None, false, 5)
        .await
        .expect("find_chunk_similarity_pairs");
}

#[tokio::test]
async fn queries_find_pattern_abstraction_pairs_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::find_pattern_abstraction_pairs(
        db.pool(),
        0.5,
        0.95,
        0.1,
        None,
        &[],
        None,
        false,
        5,
    )
    .await
    .expect("find_pattern_abstraction_pairs");
}

#[tokio::test]
async fn queries_compare_two_files_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let (a, b) = h.merge_candidate_file_ids;
    let _ = queries::compare_two_files(db.pool(), a, b, 100)
        .await
        .expect("compare_two_files");
}

#[tokio::test]
async fn queries_compare_chunks_within_file_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::compare_chunks_within_file(db.pool(), h.split_candidate_file_id, 0.5, 100)
        .await
        .expect("compare_chunks_within_file");
}

#[tokio::test]
async fn queries_resolve_file_reference_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::resolve_file_reference(db.pool(), "proj-auth:auth/file_0.rs")
        .await
        .expect("resolve_file_reference");
}

#[tokio::test]
async fn queries_resolve_file_reference_rejects_ambiguous_project_name() {
    let db = require_test_db!();
    let name = format!("dup-resolve-{}", Uuid::new_v4().simple());
    let project_a = queries::upsert_project(
        db.pool(),
        "/ws/dup-resolve-a",
        &format!("/ws/dup-resolve-a/{name}"),
        &name,
        None,
        None,
    )
    .await
    .expect("project a");
    let project_b = queries::upsert_project(
        db.pool(),
        "/ws/dup-resolve-b",
        &format!("/ws/dup-resolve-b/{name}"),
        &name,
        None,
        None,
    )
    .await
    .expect("project b");

    for (project_id, root, content_hash) in [
        (project_a, "/ws/dup-resolve-a", 31_i64),
        (project_b, "/ws/dup-resolve-b", 32_i64),
    ] {
        queries::upsert_file(
            db.pool(),
            project_id,
            &format!("{root}/{name}/src/lib.rs"),
            "src/lib.rs",
            "rust",
            1,
            Some("fn duplicate_name() {}"),
            Some(content_hash),
            1,
            false,
            false,
            Utc::now(),
        )
        .await
        .expect("file");
    }

    let resolved = queries::resolve_file_reference(db.pool(), &format!("{name}:src/lib.rs"))
        .await
        .expect("resolve ambiguous ref");
    assert!(
        resolved.is_none(),
        "duplicate project display names must fail closed"
    );
}

// =============================================================================
// Chunk content helpers (5 functions)
// =============================================================================

#[tokio::test]
async fn queries_get_chunk_content_rows_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::get_chunk_content_rows(db.pool(), &h.auth_chunk_ids[..3])
        .await
        .expect("get_chunk_content_rows");
}

#[tokio::test]
async fn queries_get_file_region_by_lines_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let path = any_file_path(db.pool()).await;
    let _ = queries::get_file_region_by_lines(db.pool(), &path, 1, 50)
        .await
        .expect("get_file_region_by_lines");
}

#[tokio::test]
async fn queries_get_chunks_in_index_range_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let path = any_file_path(db.pool()).await;
    let _ = queries::get_chunks_in_index_range(db.pool(), &path, 0, 100)
        .await
        .expect("get_chunks_in_index_range");
}

#[tokio::test]
async fn queries_file_chunk_summary_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let path = any_file_path(db.pool()).await;
    let _ = queries::file_chunk_summary(db.pool(), &path)
        .await
        .expect("file_chunk_summary");
}

#[tokio::test]
async fn queries_get_file_line_count_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let id = any_file_id(db.pool()).await;
    let _ = queries::get_file_line_count(db.pool(), id)
        .await
        .expect("get_file_line_count");
}

// =============================================================================
// Dedup / canonicalization (3 functions)
// =============================================================================

#[tokio::test]
async fn queries_find_canonical_by_content_hash_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::find_canonical_by_content_hash(db.pool(), h.auth_project_id, 0)
        .await
        .expect("find_canonical_by_content_hash");
}

#[tokio::test]
async fn queries_update_file_path_in_place_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let id = any_file_id(db.pool()).await;
    queries::update_file_path_in_place(
        db.pool(),
        id,
        "/ws/auth/renamed.rs",
        "renamed.rs",
        Utc::now(),
    )
    .await
    .expect("update_file_path_in_place");
}

#[tokio::test]
async fn queries_insert_duplicate_file_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let canonical = any_file_id(db.pool()).await;
    let _ = queries::insert_duplicate_file(
        db.pool(),
        h.auth_project_id,
        "/ws/auth/dup.rs",
        "dup.rs",
        "rust",
        42,
        0xc0fe,
        canonical,
        Utc::now(),
    )
    .await
    .expect("insert_duplicate_file");
}

// =============================================================================
// Project deletion / orphan sweep (3 functions)
// =============================================================================

#[tokio::test]
async fn queries_delete_projects_by_workspace_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::delete_projects_by_workspace(db.pool(), "/no-such-ws")
        .await
        .expect("delete_projects_by_workspace");
}

#[tokio::test]
async fn queries_cleanup_orphaned_projects_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::cleanup_orphaned_projects(db.pool())
        .await
        .expect("cleanup_orphaned_projects");
}

#[tokio::test]
async fn queries_cleanup_stale_files_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::cleanup_stale_files(db.pool())
        .await
        .expect("cleanup_stale_files");
}

// =============================================================================
// Topics / chunk-topic assignments (13 functions)
// =============================================================================

#[tokio::test]
async fn queries_clear_topics_for_scope_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    queries::clear_topics_for_scope(db.pool(), "scope-that-doesnt-exist")
        .await
        .expect("clear_topics_for_scope");
}

#[tokio::test]
async fn queries_store_topics_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    // Empty slice short-circuits — exercises entry path.
    queries::store_topics(db.pool(), "test-scope", &[])
        .await
        .expect("store_topics");
}

#[tokio::test]
async fn queries_load_cached_topics_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::load_cached_topics(db.pool(), "global", 10)
        .await
        .expect("load_cached_topics");
}

#[tokio::test]
async fn queries_find_orphan_chunks_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::find_orphan_chunks(db.pool(), Some("proj-auth"), None, 5)
        .await
        .expect("find_orphan_chunks");
}

#[tokio::test]
async fn queries_find_orphan_file_summary_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::find_orphan_file_summary(db.pool(), Some("proj-auth"))
        .await
        .expect("find_orphan_file_summary");
}

#[tokio::test]
async fn queries_load_chunk_topic_assignments_for_files_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::load_chunk_topic_assignments_for_files(db.pool(), Some("proj-auth"))
        .await
        .expect("load_chunk_topic_assignments_for_files");
}

#[tokio::test]
async fn queries_find_coupled_files_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::find_coupled_files(db.pool(), "proj-auth", 0.3, 3)
        .await
        .expect("find_coupled_files");
}

#[tokio::test]
async fn queries_get_file_complexity_data_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::get_file_complexity_data(db.pool(), "proj-auth")
        .await
        .expect("get_file_complexity_data");
}

#[tokio::test]
async fn queries_get_test_topic_coverage_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::get_test_topic_coverage(db.pool(), "proj-auth")
        .await
        .expect("get_test_topic_coverage");
}

#[tokio::test]
async fn queries_load_topic_centroids_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::load_topic_centroids(db.pool(), "global")
        .await
        .expect("load_topic_centroids");
}

#[tokio::test]
async fn queries_has_topic_assignments_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::has_topic_assignments(db.pool())
        .await
        .expect("has_topic_assignments");
}

#[tokio::test]
async fn queries_get_file_topic_distributions_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::get_file_topic_distributions(db.pool(), "proj-auth", None)
        .await
        .expect("get_file_topic_distributions");
}

#[tokio::test]
async fn queries_get_chunk_topic_details_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::get_chunk_topic_details(db.pool(), "proj-auth", None)
        .await
        .expect("get_chunk_topic_details");
}

#[tokio::test]
async fn queries_get_doc_topic_coverage_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::get_doc_topic_coverage(db.pool(), "proj-auth")
        .await
        .expect("get_doc_topic_coverage");
}

#[tokio::test]
async fn queries_get_chunk_topic_summaries_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::get_chunk_topic_summaries(db.pool(), &h.auth_chunk_ids[..3])
        .await
        .expect("get_chunk_topic_summaries");
}

// =============================================================================
// Risk / hot-paths / authorship (10 functions)
// =============================================================================

#[tokio::test]
async fn queries_count_call_sites_to_files_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let ids = vec![any_file_id(db.pool()).await];
    let _ = queries::count_call_sites_to_files(db.pool(), &ids)
        .await
        .expect("count_call_sites_to_files");
}

#[tokio::test]
async fn queries_get_file_risk_metrics_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let ids = vec![any_file_id(db.pool()).await];
    let _ = queries::get_file_risk_metrics(db.pool(), &ids)
        .await
        .expect("get_file_risk_metrics");
}

#[tokio::test]
async fn queries_find_zombie_candidates_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::find_zombie_candidates(db.pool(), "proj-auth", 30, 0.5, 5)
        .await
        .expect("find_zombie_candidates");
}

#[tokio::test]
async fn queries_get_god_file_chunks_with_topics_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::get_god_file_chunks_with_topics(db.pool(), "proj-auth", 5)
        .await
        .expect("get_god_file_chunks_with_topics");
}

#[tokio::test]
async fn queries_find_hot_paths_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::find_hot_paths(db.pool(), "proj-auth", 0.5, 5)
        .await
        .expect("find_hot_paths");
}

#[tokio::test]
async fn queries_find_bus_factor_files_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::find_bus_factor_files(db.pool(), "proj-auth", 0.5, 5)
        .await
        .expect("find_bus_factor_files");
}

#[tokio::test]
async fn queries_find_dominant_authors_for_files_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::find_dominant_authors_for_files(db.pool(), "proj-auth", &[], 90)
        .await
        .expect("find_dominant_authors_for_files");
}

#[tokio::test]
async fn queries_find_unresolved_dependencies_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::find_unresolved_dependencies(db.pool(), None, 10)
        .await
        .expect("find_unresolved_dependencies");
}

#[tokio::test]
async fn queries_find_merge_conflict_risks_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::find_merge_conflict_risks(db.pool(), "proj-auth", &[], 30, None)
        .await
        .expect("find_merge_conflict_risks");
}

#[tokio::test]
async fn queries_get_growth_buckets_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::get_growth_buckets(db.pool(), "proj-auth", None, "month", 12)
        .await
        .expect("get_growth_buckets");
}

// =============================================================================
// Embedding extraction (2 functions)
// =============================================================================

#[tokio::test]
async fn queries_bulk_extract_embeddings_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::bulk_extract_embeddings(db.pool(), None)
        .await
        .expect("bulk_extract_embeddings");
}

#[tokio::test]
async fn queries_bulk_extract_project_embeddings_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::bulk_extract_project_embeddings(db.pool(), "proj-auth", None)
        .await
        .expect("bulk_extract_project_embeddings");
}

// =============================================================================
// Symbol extraction (11 functions)
// =============================================================================

#[tokio::test]
async fn queries_list_files_for_symbol_extraction_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ =
        queries::list_files_for_symbol_extraction(db.pool(), h.auth_project_id, &["rust"], None)
            .await
            .expect("list_files_for_symbol_extraction");
}

#[tokio::test]
async fn queries_fetch_file_content_batch_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let ids = vec![any_file_id(db.pool()).await];
    let _ = queries::fetch_file_content_batch(db.pool(), h.auth_project_id, &ids)
        .await
        .expect("fetch_file_content_batch");
}

#[tokio::test]
async fn queries_delete_symbols_for_file_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let id = any_file_id(db.pool()).await;
    let _ = queries::delete_symbols_for_file(db.pool(), id)
        .await
        .expect("delete_symbols_for_file");
}

#[tokio::test]
async fn queries_delete_symbol_references_for_file_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let id = any_file_id(db.pool()).await;
    let _ = queries::delete_symbol_references_for_file(db.pool(), id)
        .await
        .expect("delete_symbol_references_for_file");
}

#[tokio::test]
async fn queries_bulk_insert_file_symbols_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let id = any_file_id(db.pool()).await;
    let _ = queries::bulk_insert_file_symbols(db.pool(), id, &[])
        .await
        .expect("bulk_insert_file_symbols");
}

#[tokio::test]
async fn queries_update_symbol_parent_ids_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::update_symbol_parent_ids(db.pool(), &[])
        .await
        .expect("update_symbol_parent_ids");
}

#[tokio::test]
async fn queries_bulk_insert_symbol_references_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let id = any_file_id(db.pool()).await;
    let _ = queries::bulk_insert_symbol_references(db.pool(), id, &[])
        .await
        .expect("bulk_insert_symbol_references");
}

#[tokio::test]
async fn queries_resolve_symbol_reference_targets_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::resolve_symbol_reference_targets(db.pool(), h.auth_project_id)
        .await
        .expect("resolve_symbol_reference_targets");
}

#[tokio::test]
async fn queries_get_symbol_extraction_watermark_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::get_symbol_extraction_watermark(db.pool(), h.auth_project_id)
        .await
        .expect("get_symbol_extraction_watermark");
}

#[tokio::test]
async fn queries_set_symbol_extraction_watermark_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    queries::set_symbol_extraction_watermark(db.pool(), h.auth_project_id, Utc::now())
        .await
        .expect("set_symbol_extraction_watermark");
}

#[tokio::test]
async fn queries_get_imports_from_symbols_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::get_imports_from_symbols(db.pool(), h.auth_project_id, &[])
        .await
        .expect("get_imports_from_symbols");
}

#[tokio::test]
async fn queries_file_ids_with_symbol_refs_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::file_ids_with_symbol_refs(db.pool(), h.auth_project_id, &[])
        .await
        .expect("file_ids_with_symbol_refs");
}

#[tokio::test]
async fn queries_get_naming_distribution_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::get_naming_distribution(db.pool(), h.auth_project_id, None)
        .await
        .expect("get_naming_distribution");
}

// =============================================================================
// Aggregate / status (1 function — the big one)
// =============================================================================

#[tokio::test]
async fn queries_status_snapshot_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let _ = queries::status_snapshot(db.pool())
        .await
        .expect("status_snapshot");
}

// Note on coverage: `common::server_with_pool` is referenced because
// `mod common;` is declared at the top, but this file doesn't actually
// use it — common is only present for the convention. Allow dead-code
// suppression via the underscore prefix in the use line is unnecessary
// since the import isn't there.
#[allow(dead_code)]
fn _common_module_is_intentionally_unused(_pool: &PgPool) {}
