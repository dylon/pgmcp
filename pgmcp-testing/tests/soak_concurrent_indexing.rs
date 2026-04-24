//! Soak tests: behavior under concurrent access.
//!
//! Uses `TestDatabase` (per-test isolation) because we need multi-connection
//! visibility — one connection per spawned task, all writing to the same
//! tables at once. The in-transaction pattern doesn't apply here.
//!
//! These tests pass the "no lost tasks" / "no deadlock" bar but don't
//! benchmark throughput. For perf numbers, the indexer's own stats surface
//! them during a live run.

use std::sync::Arc;

use pgmcp_testing::require_test_db;

/// 500 concurrent file-insert tasks against a single project — no deadlocks,
/// no lost rows. Exercises the content_hash + UNIQUE(path) code paths under
/// contention.
#[tokio::test]
async fn concurrent_file_inserts_all_land() {
    let db = require_test_db!();
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind("/ws")
    .bind("/ws/soak-proj")
    .bind("soak-proj")
    .fetch_one(db.pool())
    .await
    .expect("seed project");

    const N: usize = 500;
    let pool = Arc::new(db.pool().clone());
    let mut joinset = tokio::task::JoinSet::new();
    for i in 0..N {
        let pool = Arc::clone(&pool);
        joinset.spawn(async move {
            let path = format!("/ws/soak-proj/file_{}.rs", i);
            let content = format!("fn f{}() {{}}", i);
            let hash = xxhash_rust::xxh3::xxh3_64(content.as_bytes()) as i64;
            sqlx::query(
                "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW())"
            )
            .bind(project_id)
            .bind(&path)
            .bind(format!("file_{}.rs", i))
            .bind("rust")
            .bind(content.len() as i64)
            .bind(&content)
            .bind(hash)
            .bind(1_i32)
            .execute(&*pool)
            .await
            .expect("insert");
        });
    }
    while let Some(result) = joinset.join_next().await {
        result.expect("task complete");
    }

    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM indexed_files WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(db.pool())
            .await
            .expect("count");
    assert_eq!(count as usize, N, "some concurrent inserts were lost");
}

/// Upserts on the same path from many tasks converge to a single row.
/// Tests the `ON CONFLICT (path)` path under contention.
#[tokio::test]
async fn concurrent_upserts_of_same_path_leave_one_row() {
    let db = require_test_db!();
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind("/ws")
    .bind("/ws/soak-upsert")
    .bind("soak-upsert")
    .fetch_one(db.pool())
    .await
    .expect("seed project");

    const N: usize = 200;
    let pool = Arc::new(db.pool().clone());
    let mut joinset = tokio::task::JoinSet::new();
    for i in 0..N {
        let pool = Arc::clone(&pool);
        joinset.spawn(async move {
            let content = format!("content-rev-{}", i);
            let hash = xxhash_rust::xxh3::xxh3_64(content.as_bytes()) as i64;
            let _ = sqlx::query(
                "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW()) \
                 ON CONFLICT (path) DO UPDATE SET content = $6, content_hash = $7"
            )
            .bind(project_id)
            .bind("/ws/soak-upsert/same.rs")
            .bind("same.rs")
            .bind("rust")
            .bind(content.len() as i64)
            .bind(&content)
            .bind(hash)
            .bind(1_i32)
            .execute(&*pool)
            .await;
        });
    }
    while let Some(result) = joinset.join_next().await {
        result.expect("task complete");
    }

    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM indexed_files WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(db.pool())
            .await
            .expect("count");
    assert_eq!(
        count, 1,
        "UNIQUE(path) violated or concurrent upserts duplicated"
    );
}

/// Readers and writers interleave without deadlock. Proves the HNSW index
/// query path doesn't take a conflicting lock with the bulk insert path.
#[tokio::test]
async fn concurrent_reads_during_writes_do_not_deadlock() {
    let db = require_test_db!();
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind("/ws")
    .bind("/ws/soak-rw")
    .bind("soak-rw")
    .fetch_one(db.pool())
    .await
    .expect("seed project");
    let pool = Arc::new(db.pool().clone());

    // 50 writers, each inserting 10 files.
    let mut writers = tokio::task::JoinSet::new();
    for w in 0..50 {
        let pool = Arc::clone(&pool);
        writers.spawn(async move {
            for i in 0..10 {
                let path = format!("/ws/soak-rw/w{}_f{}.rs", w, i);
                let content = format!("fn w{}_f{}() {{}}", w, i);
                let _ = sqlx::query(
                    "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, line_count, modified_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, NOW())"
                )
                .bind(project_id)
                .bind(&path)
                .bind(format!("w{}_f{}.rs", w, i))
                .bind("rust")
                .bind(content.len() as i64)
                .bind(&content)
                .bind(1_i32)
                .execute(&*pool)
                .await;
            }
        });
    }

    // 20 readers, each doing 20 COUNT(*) queries.
    let mut readers = tokio::task::JoinSet::new();
    for _ in 0..20 {
        let pool = Arc::clone(&pool);
        readers.spawn(async move {
            for _ in 0..20 {
                let _: Result<(i64,), _> =
                    sqlx::query_as("SELECT COUNT(*) FROM indexed_files WHERE project_id = $1")
                        .bind(project_id)
                        .fetch_one(&*pool)
                        .await;
            }
        });
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while let Some(r) = writers.join_next().await {
        r.expect("writer");
        assert!(
            std::time::Instant::now() < deadline,
            "writers took > 30s — likely deadlocked"
        );
    }
    while let Some(r) = readers.join_next().await {
        r.expect("reader");
    }
    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM indexed_files WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(db.pool())
            .await
            .expect("count");
    assert_eq!(count, 500, "lost writes under contention");
}
