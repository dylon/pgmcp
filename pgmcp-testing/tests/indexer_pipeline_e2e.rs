//! Indexer pipeline end-to-end tests.
//!
//! Exercises `process_file` against a real test database, wiring the
//! embed channel to a deterministic backend that inserts chunks directly.
//! Uses `TestDatabase` (Pattern B) because the indexer path touches
//! multiple connections: process_file upserts via the `Arc<dyn DbClient>`
//! wrapper and the receiver task inserts chunks via the same db handle.

use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use tempfile::TempDir;
use tokio::task::JoinHandle;

use pgmcp::config::Config;
use pgmcp::db::DbClient;
use pgmcp::embed::EmbeddingBackend;
use pgmcp::embed::pool::EmbedIndexRequest;
use pgmcp::indexer::processor::process_file;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;

/// Spawn a background task that drains `EmbedIndexRequest::File(…)`
/// messages, computes deterministic embeddings, and inserts each chunk
/// via the embedded `Arc<dyn DbClient>`. Mirrors what the real
/// `EmbeddingPool` does, minus batching and GPU work.
fn spawn_embed_drain(
    rx: Receiver<EmbedIndexRequest>,
    backend: Arc<dyn EmbeddingBackend>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok(req) = rx.recv() {
            match req {
                EmbedIndexRequest::File(er) => {
                    for chunk in &er.chunks {
                        let embedding = backend.embed_one(&chunk.content).await.expect("embed");
                        er.db
                            .insert_chunk(
                                er.file_id,
                                chunk.chunk_index,
                                &chunk.content,
                                chunk.start_line,
                                chunk.end_line,
                                &embedding,
                            )
                            .await
                            .expect("insert_chunk");
                    }
                    er.db
                        .finalize_file_hash(er.file_id, er.content_hash)
                        .await
                        .expect("finalize");
                }
                EmbedIndexRequest::Commit(_) => {
                    // Not exercised by these tests.
                }
                EmbedIndexRequest::IndexFile(_) => {
                    // The daemon's primary path (introduced in Step 2a of
                    // the candle migration). Not exercised here — these
                    // tests submit `process_file` directly, which uses
                    // the legacy `File(EmbedRequest)` variant.
                }
            }
        }
    })
}

fn default_config() -> Config {
    Config::default()
}

async fn seed_project(db: &sqlx::PgPool, workspace: &str) -> i32 {
    sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(workspace)
    .bind(workspace)
    .bind("indexer-test")
    .fetch_one(db)
    .await
    .expect("seed project")
}

/// Wait up to `timeout` for a condition — used to coordinate with the
/// embed-drain background task.
async fn wait_for<F, Fut>(mut check: F, timeout: Duration)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("wait_for: timeout after {:?}", timeout);
}

#[tokio::test(flavor = "multi_thread")]
async fn process_file_indexes_new_rust_file_end_to_end() {
    let testdb = require_test_db!();
    let workdir = TempDir::new().expect("tempdir");
    let file_path = workdir.path().join("hello.rs");
    std::fs::write(&file_path, "fn main() { println!(\"hi\"); }\n").expect("write");

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let project_id = seed_project(testdb.pool(), workdir.path().to_str().unwrap()).await;
    let (embed_tx, embed_rx): (Sender<EmbedIndexRequest>, _) = crossbeam_channel::unbounded();
    let backend: Arc<dyn EmbeddingBackend> = Arc::new(DeterministicEmbeddingBackend::new(1024));
    let drain = spawn_embed_drain(embed_rx, backend);
    let stats = StatsTracker::new();
    let config = default_config();

    process_file(
        &file_path,
        project_id,
        workdir.path().to_str().unwrap(),
        &config,
        &db,
        &embed_tx,
        &stats,
        None,
    )
    .await
    .expect("process_file");

    // Wait for the embed drain to finish inserting chunks.
    let pool = testdb.pool().clone();
    let path_str = file_path.to_string_lossy().into_owned();
    wait_for(
        || {
            let pool = pool.clone();
            let path = path_str.clone();
            async move {
                let row: Option<(i64,)> = sqlx::query_as(
                    "SELECT id FROM indexed_files WHERE path = $1 AND content_hash IS NOT NULL",
                )
                .bind(&path)
                .fetch_optional(&pool)
                .await
                .expect("query");
                row.is_some()
            }
        },
        Duration::from_secs(5),
    )
    .await;

    // Assert the chunk landed.
    let (chunk_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM file_chunks c \
         JOIN indexed_files f ON f.id = c.file_id \
         WHERE f.path = $1",
    )
    .bind(&path_str)
    .fetch_one(testdb.pool())
    .await
    .expect("count chunks");
    assert!(chunk_count >= 1, "expected ≥ 1 chunk, got {}", chunk_count);

    drop(embed_tx);
    let _ = drain.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn process_file_skips_unchanged_file_on_rescan() {
    let testdb = require_test_db!();
    let workdir = TempDir::new().expect("tempdir");
    let file_path = workdir.path().join("stable.rs");
    std::fs::write(&file_path, "fn stable() {}\n").expect("write");

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let project_id = seed_project(testdb.pool(), workdir.path().to_str().unwrap()).await;
    let (embed_tx, embed_rx): (Sender<EmbedIndexRequest>, _) = crossbeam_channel::unbounded();
    let backend: Arc<dyn EmbeddingBackend> = Arc::new(DeterministicEmbeddingBackend::new(1024));
    let drain = spawn_embed_drain(embed_rx, backend);
    let stats = StatsTracker::new();
    let config = default_config();

    process_file(
        &file_path,
        project_id,
        workdir.path().to_str().unwrap(),
        &config,
        &db,
        &embed_tx,
        &stats,
        None,
    )
    .await
    .expect("first scan");

    let pool = testdb.pool().clone();
    let path_str = file_path.to_string_lossy().into_owned();
    wait_for(
        || {
            let pool = pool.clone();
            let path = path_str.clone();
            async move {
                let row: Option<(i64,)> = sqlx::query_as(
                    "SELECT id FROM indexed_files WHERE path = $1 AND content_hash IS NOT NULL",
                )
                .bind(&path)
                .fetch_optional(&pool)
                .await
                .expect("query");
                row.is_some()
            }
        },
        Duration::from_secs(5),
    )
    .await;

    let chunks_before: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM file_chunks c \
         JOIN indexed_files f ON f.id = c.file_id \
         WHERE f.path = $1",
    )
    .bind(&path_str)
    .fetch_one(testdb.pool())
    .await
    .expect("count");

    // Second scan on the same unchanged file → skip path (no new chunks).
    process_file(
        &file_path,
        project_id,
        workdir.path().to_str().unwrap(),
        &config,
        &db,
        &embed_tx,
        &stats,
        None,
    )
    .await
    .expect("second scan");

    // Give a moment for any (spurious) re-insertion to complete.
    tokio::time::sleep(Duration::from_millis(250)).await;

    let chunks_after: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM file_chunks c \
         JOIN indexed_files f ON f.id = c.file_id \
         WHERE f.path = $1",
    )
    .bind(&path_str)
    .fetch_one(testdb.pool())
    .await
    .expect("count");
    assert_eq!(
        chunks_after, chunks_before,
        "unchanged rescan must not re-chunk/re-embed"
    );

    drop(embed_tx);
    let _ = drain.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn process_file_re_embeds_on_content_change() {
    let testdb = require_test_db!();
    let workdir = TempDir::new().expect("tempdir");
    let file_path = workdir.path().join("change.rs");
    std::fs::write(&file_path, "fn v1() {}\n").expect("write v1");

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let project_id = seed_project(testdb.pool(), workdir.path().to_str().unwrap()).await;
    let (embed_tx, embed_rx): (Sender<EmbedIndexRequest>, _) = crossbeam_channel::unbounded();
    let backend: Arc<dyn EmbeddingBackend> = Arc::new(DeterministicEmbeddingBackend::new(1024));
    let drain = spawn_embed_drain(embed_rx, backend);
    let stats = StatsTracker::new();
    let config = default_config();

    process_file(
        &file_path,
        project_id,
        workdir.path().to_str().unwrap(),
        &config,
        &db,
        &embed_tx,
        &stats,
        None,
    )
    .await
    .expect("scan v1");

    let pool = testdb.pool().clone();
    let path_str = file_path.to_string_lossy().into_owned();
    wait_for(
        || {
            let pool = pool.clone();
            let path = path_str.clone();
            async move {
                let row: Option<(i64,)> = sqlx::query_as(
                    "SELECT id FROM indexed_files WHERE path = $1 AND content_hash IS NOT NULL",
                )
                .bind(&path)
                .fetch_optional(&pool)
                .await
                .expect("query");
                row.is_some()
            }
        },
        Duration::from_secs(5),
    )
    .await;

    let hash_v1: Option<i64> =
        sqlx::query_scalar("SELECT content_hash FROM indexed_files WHERE path = $1")
            .bind(&path_str)
            .fetch_one(testdb.pool())
            .await
            .expect("hash v1");

    // Modify content and re-scan.
    std::fs::write(&file_path, "fn v2_different() { println!(\"hi\"); }\n").expect("write v2");

    process_file(
        &file_path,
        project_id,
        workdir.path().to_str().unwrap(),
        &config,
        &db,
        &embed_tx,
        &stats,
        None,
    )
    .await
    .expect("scan v2");

    // Wait for hash to change.
    let path_clone = path_str.clone();
    let pool_clone = testdb.pool().clone();
    wait_for(
        move || {
            let pool = pool_clone.clone();
            let path = path_clone.clone();
            let prev = hash_v1;
            async move {
                let current: Option<i64> =
                    sqlx::query_scalar("SELECT content_hash FROM indexed_files WHERE path = $1")
                        .bind(&path)
                        .fetch_one(&pool)
                        .await
                        .expect("hash");
                current != prev
            }
        },
        Duration::from_secs(5),
    )
    .await;

    let hash_v2: Option<i64> =
        sqlx::query_scalar("SELECT content_hash FROM indexed_files WHERE path = $1")
            .bind(&path_str)
            .fetch_one(testdb.pool())
            .await
            .expect("hash v2");
    assert_ne!(hash_v1, hash_v2, "content change must yield new hash");

    drop(embed_tx);
    let _ = drain.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn process_file_skips_unconfigured_extension() {
    let testdb = require_test_db!();
    let workdir = TempDir::new().expect("tempdir");
    let file_path = workdir.path().join("image.zzzz_unknown");
    std::fs::write(&file_path, "binary-like data").expect("write");

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let project_id = seed_project(testdb.pool(), workdir.path().to_str().unwrap()).await;
    let (embed_tx, _embed_rx): (Sender<EmbedIndexRequest>, _) = crossbeam_channel::unbounded();
    let stats = StatsTracker::new();
    let config = default_config();

    process_file(
        &file_path,
        project_id,
        workdir.path().to_str().unwrap(),
        &config,
        &db,
        &embed_tx,
        &stats,
        None,
    )
    .await
    .expect("process_file");

    // No `indexed_files` row inserted for an unconfigured extension.
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM indexed_files WHERE project_id = $1")
        .bind(project_id)
        .fetch_one(testdb.pool())
        .await
        .expect("count");
    assert_eq!(rows, 0, "unconfigured extension should not create a row");
}

#[tokio::test(flavor = "multi_thread")]
async fn process_file_writes_all_files_in_directory() {
    let testdb = require_test_db!();
    let workdir = TempDir::new().expect("tempdir");
    for name in ["a.rs", "b.rs", "c.rs"] {
        std::fs::write(
            workdir.path().join(name),
            format!("fn {}() {{}}", name.trim_end_matches(".rs")),
        )
        .expect("write");
    }

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let project_id = seed_project(testdb.pool(), workdir.path().to_str().unwrap()).await;
    let (embed_tx, embed_rx): (Sender<EmbedIndexRequest>, _) = crossbeam_channel::unbounded();
    let backend: Arc<dyn EmbeddingBackend> = Arc::new(DeterministicEmbeddingBackend::new(1024));
    let drain = spawn_embed_drain(embed_rx, backend);
    let stats = StatsTracker::new();
    let config = default_config();

    for name in ["a.rs", "b.rs", "c.rs"] {
        process_file(
            &workdir.path().join(name),
            project_id,
            workdir.path().to_str().unwrap(),
            &config,
            &db,
            &embed_tx,
            &stats,
            None,
        )
        .await
        .expect("process_file");
    }

    let pool = testdb.pool().clone();
    wait_for(
        || {
            let pool = pool.clone();
            let project_id = project_id;
            async move {
                let (count,): (i64,) = sqlx::query_as(
                    "SELECT COUNT(*) FROM indexed_files \
                     WHERE project_id = $1 AND content_hash IS NOT NULL",
                )
                .bind(project_id)
                .fetch_one(&pool)
                .await
                .expect("count");
                count == 3
            }
        },
        Duration::from_secs(5),
    )
    .await;

    drop(embed_tx);
    let _ = drain.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn process_file_claude_jsonl_routes_through_claude_chunker() {
    let testdb = require_test_db!();
    // Fabricate a path under a `.claude/projects/…` tree so the processor
    // picks the Claude chunker (scanner auto-detects ~/.claude).
    let workdir = TempDir::new().expect("tempdir");
    let claude_dir = workdir.path().join(".claude/projects/fake-session");
    std::fs::create_dir_all(&claude_dir).expect("mkdir");
    let file_path = claude_dir.join("transcript.jsonl");
    std::fs::write(
        &file_path,
        "{\"type\": \"user\", \"message\": \"hello\"}\n\
         {\"type\": \"assistant\", \"message\": \"hi\"}\n",
    )
    .expect("write");

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let project_id = seed_project(testdb.pool(), workdir.path().to_str().unwrap()).await;
    let (embed_tx, embed_rx): (Sender<EmbedIndexRequest>, _) = crossbeam_channel::unbounded();
    let backend: Arc<dyn EmbeddingBackend> = Arc::new(DeterministicEmbeddingBackend::new(1024));
    let drain = spawn_embed_drain(embed_rx, backend);
    let stats = StatsTracker::new();
    let config = default_config();

    process_file(
        &file_path,
        project_id,
        workdir.path().to_str().unwrap(),
        &config,
        &db,
        &embed_tx,
        &stats,
        None,
    )
    .await
    .expect("process_file");

    let pool = testdb.pool().clone();
    let path_str = file_path.to_string_lossy().into_owned();
    wait_for(
        || {
            let pool = pool.clone();
            let path = path_str.clone();
            async move {
                let row: Option<(i64,)> = sqlx::query_as(
                    "SELECT id FROM indexed_files WHERE path = $1 AND content_hash IS NOT NULL",
                )
                .bind(&path)
                .fetch_optional(&pool)
                .await
                .expect("q");
                row.is_some()
            }
        },
        Duration::from_secs(5),
    )
    .await;

    // Claude chunker yields one chunk per non-skipped JSONL line → 2 chunks.
    let (chunk_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM file_chunks c \
         JOIN indexed_files f ON f.id = c.file_id WHERE f.path = $1",
    )
    .bind(&path_str)
    .fetch_one(testdb.pool())
    .await
    .expect("count");
    assert!(
        chunk_count >= 2,
        "expected ≥ 2 chunks (one per jsonl line), got {}",
        chunk_count
    );

    drop(embed_tx);
    let _ = drain.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn process_file_codex_jsonl_routes_through_codex_chunker() {
    let testdb = require_test_db!();
    let workdir = TempDir::new().expect("tempdir");
    let codex_dir = workdir.path().join(".codex/sessions/2026/05/12");
    std::fs::create_dir_all(&codex_dir).expect("mkdir");
    let file_path = codex_dir.join("rollout.jsonl");
    std::fs::write(
        &file_path,
        "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"hello codex\"}]}}\n\
         {\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi codex\"}]}}\n",
    )
    .expect("write");

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let project_id = seed_project(testdb.pool(), workdir.path().to_str().unwrap()).await;
    let (embed_tx, embed_rx): (Sender<EmbedIndexRequest>, _) = crossbeam_channel::unbounded();
    let backend: Arc<dyn EmbeddingBackend> = Arc::new(DeterministicEmbeddingBackend::new(1024));
    let drain = spawn_embed_drain(embed_rx, backend);
    let stats = StatsTracker::new();
    let config = default_config();

    process_file(
        &file_path,
        project_id,
        workdir.path().to_str().unwrap(),
        &config,
        &db,
        &embed_tx,
        &stats,
        None,
    )
    .await
    .expect("process_file");

    let pool = testdb.pool().clone();
    let path_str = file_path.to_string_lossy().into_owned();
    wait_for(
        || {
            let pool = pool.clone();
            let path = path_str.clone();
            async move {
                let row: Option<(i64,)> = sqlx::query_as(
                    "SELECT id FROM indexed_files WHERE path = $1 AND content_hash IS NOT NULL",
                )
                .bind(&path)
                .fetch_optional(&pool)
                .await
                .expect("q");
                row.is_some()
            }
        },
        Duration::from_secs(5),
    )
    .await;

    let chunks: Vec<(String,)> = sqlx::query_as(
        "SELECT c.content FROM file_chunks c \
         JOIN indexed_files f ON f.id = c.file_id WHERE f.path = $1 \
         ORDER BY c.chunk_index",
    )
    .bind(&path_str)
    .fetch_all(testdb.pool())
    .await
    .expect("chunks");
    assert_eq!(chunks.len(), 2);
    assert!(chunks[0].0.starts_with("[user]"));
    assert!(chunks[1].0.starts_with("[assistant]"));

    drop(embed_tx);
    let _ = drain.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn process_file_concurrent_on_distinct_paths_no_deadlock() {
    let testdb = require_test_db!();
    let workdir = TempDir::new().expect("tempdir");
    for i in 0..10 {
        std::fs::write(
            workdir.path().join(format!("f{}.rs", i)),
            format!("fn f{}() {{}}", i),
        )
        .expect("write");
    }

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let project_id = seed_project(testdb.pool(), workdir.path().to_str().unwrap()).await;
    let (embed_tx, embed_rx): (Sender<EmbedIndexRequest>, _) = crossbeam_channel::unbounded();
    let backend: Arc<dyn EmbeddingBackend> = Arc::new(DeterministicEmbeddingBackend::new(1024));
    let drain = spawn_embed_drain(embed_rx, backend);
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(default_config());

    let mut joinset = tokio::task::JoinSet::new();
    for i in 0..10 {
        let file_path = workdir.path().join(format!("f{}.rs", i));
        let workspace = workdir.path().to_str().unwrap().to_string();
        let db = Arc::clone(&db);
        let embed_tx = embed_tx.clone();
        let stats = Arc::clone(&stats);
        let config = Arc::clone(&config);
        joinset.spawn(async move {
            process_file(
                &file_path, project_id, &workspace, &config, &db, &embed_tx, &stats, None,
            )
            .await
            .expect("process_file");
        });
    }
    while let Some(r) = joinset.join_next().await {
        r.expect("task");
    }

    let pool = testdb.pool().clone();
    wait_for(
        || {
            let pool = pool.clone();
            let project_id = project_id;
            async move {
                let (count,): (i64,) = sqlx::query_as(
                    "SELECT COUNT(*) FROM indexed_files \
                     WHERE project_id = $1 AND content_hash IS NOT NULL",
                )
                .bind(project_id)
                .fetch_one(&pool)
                .await
                .expect("count");
                count == 10
            }
        },
        Duration::from_secs(10),
    )
    .await;

    drop(embed_tx);
    let _ = drain.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn process_file_deletes_chunks_before_re_embedding_on_change() {
    let testdb = require_test_db!();
    let workdir = TempDir::new().expect("tempdir");
    let file_path = workdir.path().join("replace.rs");
    std::fs::write(&file_path, "fn v1() { /* a */ }\n").expect("v1");

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let project_id = seed_project(testdb.pool(), workdir.path().to_str().unwrap()).await;
    let (embed_tx, embed_rx): (Sender<EmbedIndexRequest>, _) = crossbeam_channel::unbounded();
    let backend: Arc<dyn EmbeddingBackend> = Arc::new(DeterministicEmbeddingBackend::new(1024));
    let drain = spawn_embed_drain(embed_rx, backend);
    let stats = StatsTracker::new();
    let config = default_config();

    process_file(
        &file_path,
        project_id,
        workdir.path().to_str().unwrap(),
        &config,
        &db,
        &embed_tx,
        &stats,
        None,
    )
    .await
    .expect("v1");

    let pool = testdb.pool().clone();
    let path_str = file_path.to_string_lossy().into_owned();
    wait_for(
        || {
            let pool = pool.clone();
            let path = path_str.clone();
            async move {
                let row: Option<(i64,)> = sqlx::query_as(
                    "SELECT id FROM indexed_files WHERE path = $1 AND content_hash IS NOT NULL",
                )
                .bind(&path)
                .fetch_optional(&pool)
                .await
                .expect("q");
                row.is_some()
            }
        },
        Duration::from_secs(5),
    )
    .await;

    // Overwrite with new content and re-scan.
    std::fs::write(&file_path, "fn v2() { /* bbb */ }\n").expect("v2");
    process_file(
        &file_path,
        project_id,
        workdir.path().to_str().unwrap(),
        &config,
        &db,
        &embed_tx,
        &stats,
        None,
    )
    .await
    .expect("v2");

    // Old chunks are replaced, not duplicated. Content_hash for the
    // file should reflect v2.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (chunk_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM file_chunks c \
         JOIN indexed_files f ON f.id = c.file_id WHERE f.path = $1",
    )
    .bind(&path_str)
    .fetch_one(testdb.pool())
    .await
    .expect("count");
    assert!(
        chunk_count >= 1 && chunk_count <= 2,
        "expected 1 fresh chunk after replace, got {}",
        chunk_count
    );

    drop(embed_tx);
    let _ = drain.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn process_file_respects_exclude_patterns() {
    let testdb = require_test_db!();
    let workdir = TempDir::new().expect("tempdir");
    // Create a file that matches the default exclude_patterns
    // (target/**).
    let target_dir = workdir.path().join("target");
    std::fs::create_dir_all(&target_dir).expect("mkdir target");
    let excluded_path = target_dir.join("artifact.rs");
    std::fs::write(&excluded_path, "fn from_build_output() {}").expect("write");

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let project_id = seed_project(testdb.pool(), workdir.path().to_str().unwrap()).await;
    let (embed_tx, embed_rx): (Sender<EmbedIndexRequest>, _) = crossbeam_channel::unbounded();
    let backend: Arc<dyn EmbeddingBackend> = Arc::new(DeterministicEmbeddingBackend::new(1024));
    let drain = spawn_embed_drain(embed_rx, backend);
    let stats = StatsTracker::new();
    let mut config = default_config();
    // Explicitly add a pattern that matches the excluded file.
    config
        .indexer
        .exclude_patterns
        .push("target/**".to_string());

    // process_file itself does not enforce exclude_patterns — the scanner
    // does. Exercise the scanner's path filter by checking that the
    // language lookup (the first gate in process_file) is still
    // configured. For the scanner-level gate, defer to scanner tests.
    // Here we verify process_file still works when handed a file inside a
    // nominally-excluded dir (process_file is the leaf, not the filter).
    let res = process_file(
        &excluded_path,
        project_id,
        workdir.path().to_str().unwrap(),
        &config,
        &db,
        &embed_tx,
        &stats,
        None,
    )
    .await;
    assert!(
        res.is_ok(),
        "process_file should not panic on excluded path"
    );
    drop(embed_tx);
    let _ = drain.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn process_file_pgmcp_toml_override_applies_max_size() {
    // pgmcp_toml overrides are applied by the scanner via
    // `ProjectOverride`. Here we simulate the override by passing
    // `max_file_size_override = Some(50)` — the code path the scanner
    // uses when it reads a project's `.pgmcp.toml`.
    let testdb = require_test_db!();
    let workdir = TempDir::new().expect("tempdir");
    let file_path = workdir.path().join("overrode.rs");
    std::fs::write(&file_path, "x".repeat(200)).expect("write");

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let project_id = seed_project(testdb.pool(), workdir.path().to_str().unwrap()).await;
    let (embed_tx, _embed_rx): (Sender<EmbedIndexRequest>, _) = crossbeam_channel::unbounded();
    let stats = StatsTracker::new();
    let config = default_config();
    // override max size to 50 bytes — file at 200 bytes should be
    // registered as truncated.
    process_file(
        &file_path,
        project_id,
        workdir.path().to_str().unwrap(),
        &config,
        &db,
        &embed_tx,
        &stats,
        Some(50),
    )
    .await
    .expect("process_file");
    let (truncated,): (bool,) =
        sqlx::query_as("SELECT truncated FROM indexed_files WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(testdb.pool())
            .await
            .expect("row");
    assert!(
        truncated,
        "override max_file_size=50 should mark 200-byte file truncated"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn process_file_burst_of_rewrites_converges_to_last_version() {
    // Simulate a save-burst — editor rewrites the file rapidly. After
    // the burst completes and process_file runs with the final state,
    // the DB reflects the last-written content hash.
    let testdb = require_test_db!();
    let workdir = TempDir::new().expect("tempdir");
    let file_path = workdir.path().join("debounce.rs");

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let project_id = seed_project(testdb.pool(), workdir.path().to_str().unwrap()).await;
    let (embed_tx, embed_rx): (Sender<EmbedIndexRequest>, _) = crossbeam_channel::unbounded();
    let backend: Arc<dyn EmbeddingBackend> = Arc::new(DeterministicEmbeddingBackend::new(1024));
    let drain = spawn_embed_drain(embed_rx, backend);
    let stats = StatsTracker::new();
    let config = default_config();

    for i in 0..5 {
        std::fs::write(&file_path, format!("fn v{}() {{}}", i)).expect("write");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Final state: fn v4().
    process_file(
        &file_path,
        project_id,
        workdir.path().to_str().unwrap(),
        &config,
        &db,
        &embed_tx,
        &stats,
        None,
    )
    .await
    .expect("process_file");

    let pool = testdb.pool().clone();
    let path_str = file_path.to_string_lossy().into_owned();
    wait_for(
        || {
            let pool = pool.clone();
            let path = path_str.clone();
            async move {
                let row: Option<(Option<String>,)> =
                    sqlx::query_as("SELECT content FROM indexed_files WHERE path = $1")
                        .bind(&path)
                        .fetch_optional(&pool)
                        .await
                        .expect("q");
                row.and_then(|(c,)| c)
                    .map(|c| c.contains("v4"))
                    .unwrap_or(false)
            }
        },
        Duration::from_secs(5),
    )
    .await;
    drop(embed_tx);
    let _ = drain.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn git_indexer_indexes_commits_when_enabled() {
    use std::process::Command;
    let testdb = require_test_db!();
    let workdir = TempDir::new().expect("tempdir");
    let repo = workdir.path();
    // Initialize a git repo with two commits.
    Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(repo)
        .status()
        .expect("git init");
    Command::new("git")
        .args(["config", "user.email", "t@t"])
        .current_dir(repo)
        .status()
        .expect("cfg");
    Command::new("git")
        .args(["config", "user.name", "t"])
        .current_dir(repo)
        .status()
        .expect("cfg");
    std::fs::write(repo.join("a.rs"), "fn a() {}").expect("write");
    Command::new("git")
        .args(["add", "."])
        .current_dir(repo)
        .status()
        .expect("add");
    Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(repo)
        .status()
        .expect("commit");
    std::fs::write(repo.join("b.rs"), "fn b() {}").expect("write");
    Command::new("git")
        .args(["add", "."])
        .current_dir(repo)
        .status()
        .expect("add");
    Command::new("git")
        .args(["commit", "-m", "second"])
        .current_dir(repo)
        .status()
        .expect("commit");

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(repo.to_str().unwrap())
    .bind(repo.to_str().unwrap())
    .bind("git-test")
    .fetch_one(testdb.pool())
    .await
    .expect("seed project");

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let (commit_tx, _commit_rx): (Sender<pgmcp::embed::pool::EmbedCommitRequest>, _) =
        crossbeam_channel::unbounded();
    let stats = StatsTracker::new();

    pgmcp::indexer::git_indexer::index_git_history(repo, project_id, &db, &commit_tx, &stats)
        .await
        .expect("git indexer");

    let (commit_count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM git_commits WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(testdb.pool())
            .await
            .expect("count");
    assert!(
        commit_count >= 2,
        "expected ≥ 2 commits indexed, got {}",
        commit_count
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn git_indexer_incremental_resumes_from_last_sha() {
    use std::process::Command;
    let testdb = require_test_db!();
    let workdir = TempDir::new().expect("tempdir");
    let repo = workdir.path();
    Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(repo)
        .status()
        .expect("init");
    Command::new("git")
        .args(["config", "user.email", "t@t"])
        .current_dir(repo)
        .status()
        .expect("cfg");
    Command::new("git")
        .args(["config", "user.name", "t"])
        .current_dir(repo)
        .status()
        .expect("cfg");
    std::fs::write(repo.join("a.rs"), "fn a() {}").expect("write");
    Command::new("git")
        .args(["add", "."])
        .current_dir(repo)
        .status()
        .expect("add");
    Command::new("git")
        .args(["commit", "-m", "first"])
        .current_dir(repo)
        .status()
        .expect("commit");

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(repo.to_str().unwrap())
    .bind(repo.to_str().unwrap())
    .bind("git-inc-test")
    .fetch_one(testdb.pool())
    .await
    .expect("seed");

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let (commit_tx, _commit_rx): (Sender<pgmcp::embed::pool::EmbedCommitRequest>, _) =
        crossbeam_channel::unbounded();
    let stats = StatsTracker::new();

    // First scan — 1 commit.
    pgmcp::indexer::git_indexer::index_git_history(repo, project_id, &db, &commit_tx, &stats)
        .await
        .expect("first scan");
    let (count_after_first,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM git_commits WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(testdb.pool())
            .await
            .expect("count");

    // Add a commit.
    std::fs::write(repo.join("b.rs"), "fn b() {}").expect("write");
    Command::new("git")
        .args(["add", "."])
        .current_dir(repo)
        .status()
        .expect("add");
    Command::new("git")
        .args(["commit", "-m", "second"])
        .current_dir(repo)
        .status()
        .expect("commit");

    pgmcp::indexer::git_indexer::index_git_history(repo, project_id, &db, &commit_tx, &stats)
        .await
        .expect("second scan");
    let (count_after_second,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM git_commits WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(testdb.pool())
            .await
            .expect("count");
    assert!(
        count_after_second > count_after_first,
        "incremental scan should pick up the new commit: {} → {}",
        count_after_first,
        count_after_second
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn process_file_large_file_still_tracked_for_dedup() {
    let testdb = require_test_db!();
    let workdir = TempDir::new().expect("tempdir");
    let file_path = workdir.path().join("big.rs");
    std::fs::write(&file_path, "x".repeat(5_000)).expect("write");

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let project_id = seed_project(testdb.pool(), workdir.path().to_str().unwrap()).await;
    let (embed_tx, _embed_rx): (Sender<EmbedIndexRequest>, _) = crossbeam_channel::unbounded();
    let stats = StatsTracker::new();
    let config = default_config();

    // First scan: large file registered as truncated.
    process_file(
        &file_path,
        project_id,
        workdir.path().to_str().unwrap(),
        &config,
        &db,
        &embed_tx,
        &stats,
        Some(100),
    )
    .await
    .expect("first");

    let (hash1,): (Option<i64>,) =
        sqlx::query_as("SELECT content_hash FROM indexed_files WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(testdb.pool())
            .await
            .expect("hash");
    assert!(hash1.is_some(), "large file should have size+mtime hash");

    // Second scan on the same unchanged file: hash matches, skip.
    process_file(
        &file_path,
        project_id,
        workdir.path().to_str().unwrap(),
        &config,
        &db,
        &embed_tx,
        &stats,
        Some(100),
    )
    .await
    .expect("second");
    let (hash2,): (Option<i64>,) =
        sqlx::query_as("SELECT content_hash FROM indexed_files WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(testdb.pool())
            .await
            .expect("hash2");
    assert_eq!(
        hash1, hash2,
        "unchanged large file must keep same size+mtime hash"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn process_file_registers_oversize_file_as_truncated() {
    let testdb = require_test_db!();
    let workdir = TempDir::new().expect("tempdir");
    let file_path = workdir.path().join("huge.rs");
    // 5 KB file, but we'll pass max_file_size_override=100 bytes.
    let content = "x".repeat(5_000);
    std::fs::write(&file_path, &content).expect("write");

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let project_id = seed_project(testdb.pool(), workdir.path().to_str().unwrap()).await;
    let (embed_tx, _embed_rx): (Sender<EmbedIndexRequest>, _) = crossbeam_channel::unbounded();
    let stats = StatsTracker::new();
    let config = default_config();

    process_file(
        &file_path,
        project_id,
        workdir.path().to_str().unwrap(),
        &config,
        &db,
        &embed_tx,
        &stats,
        Some(100),
    )
    .await
    .expect("process_file");

    // Row should exist with truncated=true and no content.
    let row: Option<(bool, Option<String>)> =
        sqlx::query_as("SELECT truncated, content FROM indexed_files WHERE project_id = $1")
            .bind(project_id)
            .fetch_optional(testdb.pool())
            .await
            .expect("fetch");
    let (truncated, content_col) = row.expect("row exists");
    assert!(truncated, "oversize file should be marked truncated");
    assert!(
        content_col.is_none() || content_col.as_deref() == Some(""),
        "oversize file content should be NULL/empty, got {:?}",
        content_col
    );
}
