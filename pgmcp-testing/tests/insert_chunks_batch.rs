//! Followup 5 verification — `insert_chunks_batch` semantics:
//! all-or-nothing transaction, FK-violation detection without leaving
//! partial rows, empty-batch short-circuit.

use pgmcp::db::DbClient;
use pgmcp::db::queries::{ChunkInsert, insert_chunks_batch};
use pgmcp_testing::require_test_db;

#[tokio::test]
async fn empty_batch_is_a_noop_and_returns_clean_outcome() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let outcome = insert_chunks_batch(&pool, 0, &[])
        .await
        .expect("empty batch must not error");
    assert!(!outcome.fk_violation);
    assert!(outcome.error.is_none());
}

#[tokio::test]
async fn batch_inserts_all_chunks_in_one_transaction() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    // Seed a project + file to satisfy the FK.
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ('/ws', '/ws/p', 'p') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("seed project");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at)
         VALUES ($1, '/ws/p/a.rs', 'a.rs', 'rust', 100, 'fn f(){}', 1, 3, NOW())
         RETURNING id",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .expect("seed file");

    let embedding = vec![0.1_f32; 384];
    let chunks = vec![
        ChunkInsert {
            chunk_index: 0,
            content: "chunk zero",
            start_line: 1,
            end_line: 2,
            embedding: &embedding,
        },
        ChunkInsert {
            chunk_index: 1,
            content: "chunk one",
            start_line: 3,
            end_line: 4,
            embedding: &embedding,
        },
        ChunkInsert {
            chunk_index: 2,
            content: "chunk two",
            start_line: 5,
            end_line: 6,
            embedding: &embedding,
        },
    ];

    let outcome = insert_chunks_batch(&pool, file_id, &chunks)
        .await
        .expect("batch insert must succeed");
    assert!(!outcome.fk_violation);
    assert!(outcome.error.is_none());

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_chunks WHERE file_id = $1")
        .bind(file_id)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count, 3, "all 3 chunks must be present after the batch");
}

#[tokio::test]
async fn batch_rolls_back_on_fk_violation_without_partial_rows() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    // Use a file_id that does NOT exist — the very first insert will
    // hit a FK violation. The transaction rolls back, so the second
    // and third "chunks" never materialize either. The outcome flags
    // `fk_violation = true` so the caller knows to log it as such
    // rather than as an opaque error.
    let nonexistent_file_id: i64 = 99_999_999;
    let embedding = vec![0.2_f32; 384];
    let chunks = vec![
        ChunkInsert {
            chunk_index: 0,
            content: "should not land",
            start_line: 1,
            end_line: 1,
            embedding: &embedding,
        },
        ChunkInsert {
            chunk_index: 1,
            content: "also should not land",
            start_line: 2,
            end_line: 2,
            embedding: &embedding,
        },
    ];

    let outcome = insert_chunks_batch(&pool, nonexistent_file_id, &chunks)
        .await
        .expect("transport-level errors should not bubble — sqlx returns Ok with the FK info");
    assert!(outcome.fk_violation, "FK violation must be flagged");
    assert!(
        outcome.error.is_none(),
        "FK case is signalled via the flag, not the error field"
    );

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_chunks WHERE file_id = $1")
        .bind(nonexistent_file_id)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(
        count, 0,
        "rollback must leave zero rows for the bogus file_id"
    );
}

#[tokio::test]
async fn batch_via_trait_routes_through_pgpool_impl() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let db_client: std::sync::Arc<dyn DbClient> = std::sync::Arc::new(pool.clone());

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ('/ws2', '/ws2/p', 'p2') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("seed project");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at)
         VALUES ($1, '/ws2/p/b.rs', 'b.rs', 'rust', 50, 'fn g(){}', 2, 2, NOW())
         RETURNING id",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .expect("seed file");

    let embedding = vec![0.5_f32; 384];
    let outcome = db_client
        .insert_chunks_batch(
            file_id,
            &[ChunkInsert {
                chunk_index: 0,
                content: "via trait",
                start_line: 1,
                end_line: 1,
                embedding: &embedding,
            }],
        )
        .await
        .expect("trait dispatch must succeed");
    assert!(!outcome.fk_violation);
    assert!(outcome.error.is_none());
}
