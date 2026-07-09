//! Regression tests for the two `fuzzy-sync` rebuild bugs:
//!
//! - **NULL visibility** — `file_symbols.visibility` is nullable; the symbol
//!   extractor leaves it NULL for symbols whose visibility it can't determine.
//!   `rebuild_symbols` must `COALESCE(fs.visibility, 'private')` so the
//!   non-`Option` tuple slot never decodes a NULL (the original bug:
//!   `error occurred while decoding column 3: unexpected null`).
//!
//! - **`sha` vs `commit_hash`** — `git_commits` has no `sha` column; the
//!   commit-hash column is `commit_hash`. `rebuild_commits` must select
//!   `commit_hash` (the original query failed at plan time with
//!   `column "sha" does not exist`). This was masked in production because
//!   `rebuild_symbols` `?`-aborts the cron before `rebuild_commits` runs.
//!
//! The pre-existing fuzzy seed helper (`tool_fuzzy_search_uses_persistent_trie`)
//! always inserts `visibility = 'public'` and never seeds commits, which is
//! exactly why both bugs escaped CI.

use pgmcp::cron::fuzzy_sync::{project_artifact_key, trie_path};
use pgmcp::fuzzy::persistent_artrie::FuzzyIndex;
use pgmcp::fuzzy::sync::{rebuild_commits, rebuild_symbols};
use pgmcp::fuzzy::values::{CommitRef, SymbolValue};
use pgmcp_testing::require_test_db;

/// Seed a project + one indexed file + one `file_symbols` row inserted
/// WITHOUT the `visibility` column, leaving it NULL. Returns `(project_id, file_id)`.
async fn seed_project_with_null_visibility_symbol(
    pool: &sqlx::PgPool,
    project_name: &str,
    symbol_name: &str,
) -> (i32, i64) {
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET workspace_path = $1 RETURNING id",
    )
    .bind(format!("/ws/{project_name}"))
    .bind(format!("/ws/{project_name}/proj"))
    .bind(project_name)
    .fetch_one(pool)
    .await
    .expect("project");

    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at) \
         VALUES ($1, $2, $3, 'rust', $4, $5, $6, $7, NOW()) \
         ON CONFLICT (path) DO UPDATE SET content = $5 RETURNING id",
    )
    .bind(project_id)
    .bind(format!("/ws/{project_name}/proj/src/lib.rs"))
    .bind("src/lib.rs")
    .bind(1024_i64)
    .bind("seed")
    .bind(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0)
            ^ (project_name.len() as i64),
    )
    .bind(10_i32)
    .fetch_one(pool)
    .await
    .expect("file");

    // NOTE: `visibility` (and `signature`, `parent_id`) intentionally omitted →
    // they default to NULL. This is the exact condition that triggered Bug 1.
    sqlx::query(
        "INSERT INTO file_symbols (file_id, name, kind, start_line, end_line) \
         VALUES ($1, $2, 'function', 1, 1) ON CONFLICT DO NOTHING",
    )
    .bind(file_id)
    .bind(symbol_name)
    .execute(pool)
    .await
    .expect("symbol");

    (project_id, file_id)
}

async fn seed_commit(pool: &sqlx::PgPool, project_id: i32, subject: &str, commit_hash: &str) {
    sqlx::query(
        "INSERT INTO git_commits (project_id, commit_hash, author, author_date, subject, body) \
         VALUES ($1, $2, $3, NOW(), $4, $5) \
         ON CONFLICT (project_id, commit_hash) DO UPDATE SET subject = $4",
    )
    .bind(project_id)
    .bind(commit_hash)
    .bind("Test Author")
    .bind(subject)
    .bind("body text")
    .execute(pool)
    .await
    .expect("commit");
}

#[tokio::test(flavor = "multi_thread")]
async fn rebuild_symbols_tolerates_null_visibility() {
    let testdb = require_test_db!();
    let project = "fuzzy_sync_nullvis";
    let (project_id, _file_id) =
        seed_project_with_null_visibility_symbol(testdb.pool(), project, "null_vis_symbol").await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let path = trie_path(
        tmp.path(),
        "symbols",
        &project_artifact_key(project_id, project),
    );
    let (idx, _recovery) =
        FuzzyIndex::<SymbolValue>::open_or_create(&path).expect("open_or_create");

    // Pre-fix this returned: "persistent trie error: symbol fetch: error occurred
    // while decoding column 3: unexpected null; try decoding as an `Option`".
    let count = rebuild_symbols(testdb.pool(), project_id, &idx, 25_000)
        .await
        .expect("rebuild_symbols must tolerate NULL visibility");
    assert!(count >= 1, "expected >=1 symbol synced, got {count}");

    let value = idx
        .get("null_vis_symbol")
        .expect("seeded symbol must be present in the trie");
    assert_eq!(
        value.visibility, "private",
        "NULL visibility must COALESCE to 'private'"
    );
    assert_eq!(value.kind, "function");
}

#[tokio::test(flavor = "multi_thread")]
async fn rebuild_commits_uses_commit_hash_column() {
    let testdb = require_test_db!();
    let project = "fuzzy_sync_commits";
    let (project_id, _file_id) =
        seed_project_with_null_visibility_symbol(testdb.pool(), project, "any_symbol").await;
    seed_commit(testdb.pool(), project_id, "Fix the thing", "abc123def456").await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let path = trie_path(
        tmp.path(),
        "commits",
        &project_artifact_key(project_id, project),
    );
    let (idx, _recovery) = FuzzyIndex::<CommitRef>::open_or_create(&path).expect("open_or_create");

    // Pre-fix this returned: "commit fetch: ... column \"sha\" does not exist".
    let count = rebuild_commits(testdb.pool(), project_id, &idx, 25_000)
        .await
        .expect("rebuild_commits must select commit_hash, not sha");
    assert!(count >= 1, "expected >=1 commit synced, got {count}");

    let value = idx
        .get("Fix the thing")
        .expect("seeded commit subject must be present in the trie");
    assert_eq!(
        value.sha, "abc123def456",
        "CommitRef.sha must carry git_commits.commit_hash"
    );
    assert_eq!(value.project_id, project_id);
}
