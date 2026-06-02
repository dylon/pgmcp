//! RC2 regression — symbol extraction recovers content-NULL files from disk.
//!
//! The asymmetric-storage policy stores `indexed_files.content = NULL` for
//! plain-text files cheap to re-read from disk (keeping only `content_hash`).
//! The extraction cron must recover such a file's text from disk — verified
//! against `content_hash` — and parse it, instead of skipping it. The old
//! `content IS NOT NULL` gate left ~90% of an actively-reindexed project (incl.
//! pgmcp's own `src/`) unextracted. A hash mismatch (file edited since
//! indexing) must skip the file AND leave the watermark before it (F1) so the
//! next run retries it.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use pgmcp::cron::symbol_extraction;
use pgmcp::db::DbClient;
use pgmcp::db::disk_read::content_hash_i64;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::require_test_db;
use sqlx::PgPool;

async fn seed_project(pool: &PgPool, name: &str, path: &str) -> i32 {
    sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET name = $3 RETURNING id",
    )
    .bind("/ws")
    .bind(path)
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("project")
}

/// Insert a content-NULL row whose bytes live on disk at `abs_path`, with the
/// given `content_hash` (pass a deliberately wrong value to exercise the
/// mismatch path).
async fn seed_disk_backed_file(
    pool: &PgPool,
    project_id: i32,
    abs_path: &str,
    relative_path: &str,
    content_hash: i64,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO indexed_files
            (project_id, path, relative_path, language, size_bytes, content,
             content_recoverable_from_disk, content_hash, line_count, modified_at)
         VALUES ($1, $2, $3, 'rust', 1, NULL, TRUE, $4, 1, NOW())
         ON CONFLICT (path) DO UPDATE SET
            content = NULL, content_recoverable_from_disk = TRUE,
            content_hash = $4, modified_at = NOW()
         RETURNING id",
    )
    .bind(project_id)
    .bind(abs_path)
    .bind(relative_path)
    .bind(content_hash)
    .fetch_one(pool)
    .await
    .expect("file")
}

fn scratch_file(tag: &str, body: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("pgmcp_rc2_{}_{}.rs", std::process::id(), tag));
    std::fs::write(&path, body).expect("write scratch source");
    path
}

#[tokio::test(flavor = "multi_thread")]
async fn content_null_on_disk_file_is_extracted_from_disk() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = seed_project(&pool, "rc2-disk", "/ws/rc2-disk").await;

    let body = "pub fn alpha_beta_gamma() {}\n";
    let path = scratch_file("hit", body);
    let abs = path.to_str().expect("utf8 path").to_string();
    let file_id = seed_disk_backed_file(
        &pool,
        project_id,
        &abs,
        "src/alpha.rs",
        content_hash_i64(body.as_bytes()),
    )
    .await;

    let db_client: Arc<dyn DbClient> = Arc::new(pool.clone());
    let stats = Arc::new(StatsTracker::new());
    symbol_extraction::run_symbol_extraction_for_project(db_client.as_ref(), &stats, "rc2-disk")
        .await;
    let _ = std::fs::remove_file(&path);

    let symbol_present: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM file_symbols WHERE file_id = $1 AND name = 'alpha_beta_gamma')",
    )
    .bind(file_id)
    .fetch_one(&pool)
    .await
    .expect("symbol lookup");
    assert!(
        symbol_present,
        "a content-NULL on-disk file must be extracted via the hash-verified disk fast-path"
    );
    assert!(
        stats.symbol_extraction_disk_reads.load(Ordering::Acquire) >= 1,
        "expected at least one hash-verified disk read counter increment"
    );

    // Incremental-skip (RC2 (c)): force a full re-scan (clear the watermark) and
    // re-run — the unchanged file's content_hash now equals its
    // extracted_content_hash, so it is skipped without a re-parse.
    sqlx::query("DELETE FROM pgmcp_metadata WHERE key = 'symbol_extraction_last_run:' || $1::text")
        .bind(project_id)
        .execute(&pool)
        .await
        .expect("clear watermark");
    let stats2 = Arc::new(StatsTracker::new());
    let path2 = scratch_file("hit", body); // recreate on disk for the (skipped) read path
    symbol_extraction::run_symbol_extraction_for_project(db_client.as_ref(), &stats2, "rc2-disk")
        .await;
    let _ = std::fs::remove_file(&path2);
    assert!(
        stats2
            .symbol_extraction_unchanged_skips
            .load(Ordering::Acquire)
            >= 1,
        "an unchanged file must be incremental-skipped on a full re-scan"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn content_null_hash_mismatch_is_skipped_and_not_watermarked() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = seed_project(&pool, "rc2-mismatch", "/ws/rc2-mismatch").await;

    let body = "pub fn delta_epsilon() {}\n";
    let path = scratch_file("mismatch", body);
    let abs = path.to_str().expect("utf8 path").to_string();
    // Deliberately wrong hash → the disk read must be rejected as a mismatch.
    let file_id = seed_disk_backed_file(&pool, project_id, &abs, "src/delta.rs", 0x0bad_c0de).await;
    let modified_at: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT modified_at FROM indexed_files WHERE id = $1")
            .bind(file_id)
            .fetch_one(&pool)
            .await
            .expect("modified_at");

    let db_client: Arc<dyn DbClient> = Arc::new(pool.clone());
    let stats = Arc::new(StatsTracker::new());
    symbol_extraction::run_symbol_extraction_for_project(
        db_client.as_ref(),
        &stats,
        "rc2-mismatch",
    )
    .await;
    let _ = std::fs::remove_file(&path);

    let symbol_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM file_symbols WHERE file_id = $1")
            .bind(file_id)
            .fetch_one(&pool)
            .await
            .expect("symbol count");
    assert_eq!(
        symbol_count, 0,
        "a hash-mismatch file must yield no symbols"
    );
    assert!(
        stats
            .symbol_extraction_disk_hash_mismatches
            .load(Ordering::Acquire)
            >= 1,
        "expected a disk hash-mismatch to be counted"
    );

    // F1: the watermark must stay strictly before the skipped file's
    // modified_at, so the file is re-listed (and retried) on the next run.
    let watermark: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT value::timestamptz FROM pgmcp_metadata
         WHERE key = 'symbol_extraction_last_run:' || $1::text",
    )
    .bind(project_id)
    .fetch_optional(&pool)
    .await
    .expect("watermark query");
    assert!(
        matches!(watermark, Some(wm) if wm < modified_at),
        "F1: watermark must stay before the skipped file's modified_at; got {watermark:?} vs {modified_at}"
    );
}
