//! Real-DB test for the boyscout bug-gate query (ADR-022):
//! `open_bugs_anchored_to_paths` must surface OPEN `kind='bug'` work-items
//! anchored (via `work_item_code_anchor`) to a touched path, and must NOT
//! surface terminal (verified/cancelled/deferred) bugs. Self-skips without a
//! configured test database.

use pgmcp::db::queries::open_bugs_anchored_to_paths;
use pgmcp_testing::require_test_db;
use sqlx::PgPool;

async fn seed_project(pool: &PgPool, name: &str, path: &str) -> i32 {
    sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ('/ws', $1, $2)
         ON CONFLICT (path) DO UPDATE SET name = $2 RETURNING id",
    )
    .bind(path)
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("project")
}

async fn seed_file(pool: &PgPool, project_id: i32, abs: &str, rel: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO indexed_files
            (project_id, path, relative_path, language, size_bytes, content,
             content_recoverable_from_disk, content_hash, line_count, modified_at)
         VALUES ($1, $2, $3, 'rust', 1, 'x', false, 1, 1, NOW())
         ON CONFLICT (path) DO UPDATE SET relative_path = $3 RETURNING id",
    )
    .bind(project_id)
    .bind(abs)
    .bind(rel)
    .fetch_one(pool)
    .await
    .expect("file")
}

async fn seed_bug(pool: &PgPool, project_id: i32, public_id: &str, status: &str, sev: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO work_items (public_id, project_id, kind, status, title, severity)
         VALUES ($1, $2, 'bug', $3, $4, $5)
         ON CONFLICT (public_id) DO UPDATE SET status = $3 RETURNING id",
    )
    .bind(public_id)
    .bind(project_id)
    .bind(status)
    .bind(format!("bug {public_id}"))
    .bind(sev)
    .fetch_one(pool)
    .await
    .expect("bug")
}

async fn anchor(pool: &PgPool, item_id: i64, file_id: i64) {
    sqlx::query(
        "INSERT INTO work_item_code_anchor (item_id, file_id, anchor_type)
         VALUES ($1, $2, 'bug')",
    )
    .bind(item_id)
    .bind(file_id)
    .execute(pool)
    .await
    .expect("anchor");
}

#[tokio::test(flavor = "multi_thread")]
async fn open_bug_anchored_to_path_is_found_terminal_is_not() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    let pid = seed_project(&pool, "buggate", "/ws/buggate").await;
    // A deliberately unique relative_path so the project-agnostic suffix match
    // cannot collide with another test's seeded files in the shared template DB.
    let rel = "src/buggate_widget_zzz.rs";
    let fid = seed_file(&pool, pid, "/ws/buggate/src/buggate_widget_zzz.rs", rel).await;

    let open_id = seed_bug(&pool, pid, "buggate-open-zzz", "in_progress", "high").await;
    anchor(&pool, open_id, fid).await;
    let verified_id = seed_bug(&pool, pid, "buggate-verified-zzz", "verified", "low").await;
    anchor(&pool, verified_id, fid).await;

    // The touched path matches the file (exact, and by suffix): the open bug is
    // flagged; the verified bug is not.
    let hits = open_bugs_anchored_to_paths(&pool, &[rel.to_string()], 50)
        .await
        .expect("query");
    assert!(
        hits.iter().any(|h| h.public_id == "buggate-open-zzz"),
        "an open bug anchored to a touched file must be flagged; got {hits:?}"
    );
    assert!(
        !hits.iter().any(|h| h.public_id == "buggate-verified-zzz"),
        "a verified bug must NOT be flagged (it is terminal)"
    );

    // A path matching nothing yields no hit for our bug.
    let none = open_bugs_anchored_to_paths(&pool, &["src/no_such_file_qqq.rs".to_string()], 50)
        .await
        .expect("query");
    assert!(!none.iter().any(|h| h.public_id == "buggate-open-zzz"));

    // Empty input short-circuits to empty (no query executed).
    let empty = open_bugs_anchored_to_paths(&pool, &[], 50)
        .await
        .expect("query");
    assert!(empty.is_empty());
}
