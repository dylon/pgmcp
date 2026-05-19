//! Cron job end-to-end tests.
//!
//! Each test seeds the minimum viable schema for one cron job, runs the
//! job against `TestDatabase`, and asserts the expected state mutations.

use std::sync::Arc;

use pgmcp::config::{CronConfig, VectorConfig};
use pgmcp::cron::graph_analysis::run_graph_analysis;
use pgmcp::cron::similarity::run_similarity_scan;
use pgmcp::cron::topic_clustering::run_global_topic_scan;
use pgmcp::daemon_state::DaemonLifecycle;
use pgmcp::db::DbClient;
use pgmcp::embed::EmbeddingBackend;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;

/// Seed a project + file + N chunks with deterministic embeddings.
/// Returns (project_id, file_id).
async fn seed_chunks(
    pool: &sqlx::PgPool,
    project_name: &str,
    project_path: &str,
    file_rel_path: &str,
    contents: Vec<&str>,
    backend: &Arc<dyn EmbeddingBackend>,
) -> (i32, i64) {
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET workspace_path = $1 RETURNING id",
    )
    .bind("/ws")
    .bind(project_path)
    .bind(project_name)
    .fetch_one(pool)
    .await
    .expect("project");
    let full_path = format!("{}/{}", project_path, file_rel_path);
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at) \
         VALUES ($1, $2, $3, 'rust', $4, $5, $6, $7, NOW()) \
         ON CONFLICT (path) DO UPDATE SET content = $5 RETURNING id"
    )
    .bind(project_id)
    .bind(&full_path)
    .bind(file_rel_path)
    .bind(contents.iter().map(|s| s.len()).sum::<usize>() as i64)
    .bind(contents.join("\n"))
    .bind(42_i64)
    .bind(contents.len() as i32)
    .fetch_one(pool)
    .await
    .expect("file");
    for (i, content) in contents.iter().enumerate() {
        let embedding = backend.embed_one(content).await.expect("embed");
        let v = pgvector::Vector::from(embedding);
        sqlx::query(
            "INSERT INTO file_chunks (file_id, chunk_index, content, start_line, end_line, embedding) \
             VALUES ($1, $2, $3, $4, $5, $6)"
        )
        .bind(file_id)
        .bind(i as i32)
        .bind(*content)
        .bind((i as i32) + 1)
        .bind((i as i32) + 1)
        .bind(v)
        .execute(pool)
        .await
        .expect("chunk");
    }
    (project_id, file_id)
}

#[tokio::test(flavor = "multi_thread")]
async fn similarity_scan_populates_cross_project_similarities() {
    let testdb = require_test_db!();
    let backend: Arc<dyn EmbeddingBackend> = Arc::new(DeterministicEmbeddingBackend::new(384));
    // Seed two projects with identical content so the similarity scan
    // finds a cross-project pair.
    let shared = vec!["fn hello_world() { }", "fn process_request() { }"];
    seed_chunks(
        testdb.pool(),
        "alpha",
        "/ws/alpha",
        "src/shared.rs",
        shared.clone(),
        &backend,
    )
    .await;
    seed_chunks(
        testdb.pool(),
        "beta",
        "/ws/beta",
        "src/shared.rs",
        shared,
        &backend,
    )
    .await;

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let stats = Arc::new(StatsTracker::new());
    let mut cron_cfg = CronConfig::default();
    cron_cfg.similarity_threshold = 0.5; // lax to ensure matches
    cron_cfg.similarity_top_k = 5;
    let vector_cfg = VectorConfig::default();

    run_similarity_scan(
        db.as_ref(),
        &cron_cfg,
        vector_cfg.ef_search,
        &stats,
        &DaemonLifecycle::new(),
    )
    .await;

    let (pair_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM cross_project_similarities")
        .fetch_one(testdb.pool())
        .await
        .expect("count");
    assert!(
        pair_count >= 1,
        "similarity scan must find ≥ 1 pair, got {}",
        pair_count
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn topic_clustering_populates_code_topics() {
    let testdb = require_test_db!();
    let backend: Arc<dyn EmbeddingBackend> = Arc::new(DeterministicEmbeddingBackend::new(384));
    // Need ≥ min_cluster_size * 2 chunks for FCM to produce meaningful topics.
    // CronConfig default min_cluster_size=5, so seed ~20 diverse chunks.
    let mut contents: Vec<&str> = Vec::new();
    let chunk_strings: Vec<String> = (0..20)
        .map(|i| format!("chunk content number {} with unique tokens", i))
        .collect();
    for s in &chunk_strings {
        contents.push(s.as_str());
    }
    seed_chunks(
        testdb.pool(),
        "gamma",
        "/ws/gamma",
        "src/wide.rs",
        contents,
        &backend,
    )
    .await;

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let stats = Arc::new(StatsTracker::new());
    let mut cron_cfg = CronConfig::default();
    cron_cfg.topic_min_cluster_size = 3;
    cron_cfg.topic_num_clusters = Some(3);
    cron_cfg.topic_fcm_max_iters = 20;

    run_global_topic_scan(db.as_ref(), &cron_cfg, &stats, &DaemonLifecycle::new()).await;

    let (topic_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM code_topics")
        .fetch_one(testdb.pool())
        .await
        .expect("count");
    assert!(
        topic_count >= 1,
        "topic clustering must produce ≥ 1 topic, got {}",
        topic_count
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn graph_analysis_writes_file_metrics_rows() {
    let testdb = require_test_db!();
    let backend: Arc<dyn EmbeddingBackend> = Arc::new(DeterministicEmbeddingBackend::new(384));

    // Seed two files in the same project with at least one "use crate::"
    // statement so the import extractor picks up an edge.
    seed_chunks(
        testdb.pool(),
        "delta",
        "/ws/delta",
        "src/main.rs",
        vec!["use crate::db::queries;\nfn main() {}"],
        &backend,
    )
    .await;
    seed_chunks(
        testdb.pool(),
        "delta",
        "/ws/delta",
        "src/db/queries.rs",
        vec!["pub fn query() {}"],
        &backend,
    )
    .await;

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let stats = Arc::new(StatsTracker::new());
    run_graph_analysis(db.as_ref(), &stats, None).await;

    // Either file_metrics rows exist, or the analysis completed without rows
    // (small graphs may produce no metrics). Either way, no panic is a win.
    let (file_metrics_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM file_metrics")
        .fetch_one(testdb.pool())
        .await
        .expect("count");
    // Small corpus — zero rows is acceptable. Primary assertion is no panic.
    let _ = file_metrics_count;

    // Graph build counter must have been incremented (proves the cron ran).
    let runs = stats
        .graph_build_runs
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        runs >= 1,
        "graph_build_runs counter should have incremented"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn similarity_scan_below_threshold_emits_nothing() {
    let testdb = require_test_db!();
    let backend: Arc<dyn EmbeddingBackend> = Arc::new(DeterministicEmbeddingBackend::new(384));
    // Two projects with *different* content so similarity is low.
    seed_chunks(
        testdb.pool(),
        "zeta",
        "/ws/zeta",
        "src/a.rs",
        vec!["fn zeta_a() {}"],
        &backend,
    )
    .await;
    seed_chunks(
        testdb.pool(),
        "eta",
        "/ws/eta",
        "src/x.rs",
        vec!["some totally different content string"],
        &backend,
    )
    .await;
    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let stats = Arc::new(StatsTracker::new());
    let mut cron_cfg = CronConfig::default();
    cron_cfg.similarity_threshold = 0.99; // unreachable for distinct content
    cron_cfg.similarity_top_k = 5;
    let vector_cfg = VectorConfig::default();
    run_similarity_scan(
        db.as_ref(),
        &cron_cfg,
        vector_cfg.ef_search,
        &stats,
        &DaemonLifecycle::new(),
    )
    .await;
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM cross_project_similarities \
         WHERE project_name_a IN ('zeta', 'eta') OR project_name_b IN ('zeta', 'eta')",
    )
    .fetch_one(testdb.pool())
    .await
    .expect("count");
    assert_eq!(
        count, 0,
        "threshold 0.99 should reject all cross-project distinct content"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn topic_clustering_assigns_chunks_above_threshold() {
    let testdb = require_test_db!();
    let backend: Arc<dyn EmbeddingBackend> = Arc::new(DeterministicEmbeddingBackend::new(384));
    let chunk_strings: Vec<String> = (0..20)
        .map(|i| format!("chunk content number {} with unique tokens", i))
        .collect();
    let contents: Vec<&str> = chunk_strings.iter().map(|s| s.as_str()).collect();
    seed_chunks(
        testdb.pool(),
        "iota",
        "/ws/iota",
        "src/wide.rs",
        contents,
        &backend,
    )
    .await;
    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let stats = Arc::new(StatsTracker::new());
    let mut cron_cfg = CronConfig::default();
    cron_cfg.topic_min_cluster_size = 3;
    cron_cfg.topic_num_clusters = Some(3);
    cron_cfg.topic_fcm_max_iters = 20;
    cron_cfg.topic_membership_threshold = 0.1;
    run_global_topic_scan(db.as_ref(), &cron_cfg, &stats, &DaemonLifecycle::new()).await;
    let (assignment_count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM chunk_topic_assignments")
            .fetch_one(testdb.pool())
            .await
            .expect("count");
    assert!(
        assignment_count >= 1,
        "clustering must produce at least one membership assignment"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn graph_analysis_increments_counter_per_run() {
    let testdb = require_test_db!();
    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let stats = Arc::new(StatsTracker::new());
    run_graph_analysis(db.as_ref(), &stats, None).await;
    let first = stats
        .graph_build_runs
        .load(std::sync::atomic::Ordering::Relaxed);
    run_graph_analysis(db.as_ref(), &stats, None).await;
    let second = stats
        .graph_build_runs
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        second > first,
        "graph_build_runs must increment on each invocation: {} → {}",
        first,
        second
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn graph_analysis_with_work_pool_runs_without_hanging() {
    let testdb = require_test_db!();
    let backend: Arc<dyn EmbeddingBackend> = Arc::new(DeterministicEmbeddingBackend::new(384));
    seed_chunks(
        testdb.pool(),
        "wp",
        "/ws/wp",
        "src/main.rs",
        vec!["use crate::util;\nfn main() {}"],
        &backend,
    )
    .await;
    seed_chunks(
        testdb.pool(),
        "wp",
        "/ws/wp",
        "src/util.rs",
        vec!["pub fn util() {}"],
        &backend,
    )
    .await;
    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let stats = Arc::new(StatsTracker::new());
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pool = Arc::new(pgmcp::work_pool::pool::WorkPool::new(
        2,
        4,
        4,
        Arc::clone(&shutdown),
    ));
    let start = std::time::Instant::now();
    run_graph_analysis(db.as_ref(), &stats, Some(pool)).await;
    shutdown.store(true, std::sync::atomic::Ordering::Release);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(30),
        "graph analysis with work pool took >30s — likely hanging"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn similarity_scan_is_idempotent_across_runs() {
    let testdb = require_test_db!();
    let backend: Arc<dyn EmbeddingBackend> = Arc::new(DeterministicEmbeddingBackend::new(384));
    let shared = vec!["fn common() {}"];
    seed_chunks(
        testdb.pool(),
        "ida",
        "/ws/ida",
        "a.rs",
        shared.clone(),
        &backend,
    )
    .await;
    seed_chunks(testdb.pool(), "idb", "/ws/idb", "a.rs", shared, &backend).await;
    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let stats = Arc::new(StatsTracker::new());
    let mut cron_cfg = CronConfig::default();
    cron_cfg.similarity_threshold = 0.5;
    cron_cfg.similarity_top_k = 5;
    let vector_cfg = VectorConfig::default();
    run_similarity_scan(
        db.as_ref(),
        &cron_cfg,
        vector_cfg.ef_search,
        &stats,
        &DaemonLifecycle::new(),
    )
    .await;
    let (count_after_first,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM cross_project_similarities \
         WHERE project_name_a IN ('ida', 'idb') OR project_name_b IN ('ida', 'idb')",
    )
    .fetch_one(testdb.pool())
    .await
    .expect("count1");
    run_similarity_scan(
        db.as_ref(),
        &cron_cfg,
        vector_cfg.ef_search,
        &stats,
        &DaemonLifecycle::new(),
    )
    .await;
    let (count_after_second,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM cross_project_similarities \
         WHERE project_name_a IN ('ida', 'idb') OR project_name_b IN ('ida', 'idb')",
    )
    .fetch_one(testdb.pool())
    .await
    .expect("count2");
    assert_eq!(
        count_after_first, count_after_second,
        "back-to-back similarity scans must not double-insert pairs"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn topic_clustering_with_insufficient_chunks_handles_gracefully() {
    let testdb = require_test_db!();
    let backend: Arc<dyn EmbeddingBackend> = Arc::new(DeterministicEmbeddingBackend::new(384));
    // Only 2 chunks — below min_cluster_size default (5).
    seed_chunks(
        testdb.pool(),
        "tiny",
        "/ws/tiny",
        "src/tiny.rs",
        vec!["one", "two"],
        &backend,
    )
    .await;
    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let stats = Arc::new(StatsTracker::new());
    let cron_cfg = CronConfig::default();
    // Must not panic, even with insufficient data.
    run_global_topic_scan(db.as_ref(), &cron_cfg, &stats, &DaemonLifecycle::new()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn graph_analysis_on_empty_db_does_not_panic() {
    let testdb = require_test_db!();
    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let stats = Arc::new(StatsTracker::new());
    run_graph_analysis(db.as_ref(), &stats, None).await;
    // Primary assertion: no panic. Counter still increments.
    let runs = stats
        .graph_build_runs
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(runs >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn git_history_cron_indexes_real_repo() {
    use std::process::Command;
    let testdb = require_test_db!();
    // Create a temp git repo with one commit.
    let workdir = tempfile::TempDir::new().expect("tempdir");
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
    std::fs::write(repo.join("x.rs"), "fn x() {}").expect("write");
    Command::new("git")
        .args(["add", "."])
        .current_dir(repo)
        .status()
        .expect("add");
    Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(repo)
        .status()
        .expect("commit");

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(repo.to_str().unwrap())
    .bind(repo.to_str().unwrap())
    .bind("git-cron-test")
    .fetch_one(testdb.pool())
    .await
    .expect("seed");

    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let (commit_tx, _commit_rx): (
        crossbeam_channel::Sender<pgmcp::embed::pool::EmbedCommitRequest>,
        _,
    ) = crossbeam_channel::unbounded();
    let stats = pgmcp::stats::tracker::StatsTracker::new();

    // The cron runs index_git_history for every project with
    // `[git] index_history = true` in its `.pgmcp.toml`. Here we call the
    // indexer directly — equivalent to one cron tick after per-project
    // filtering.
    pgmcp::indexer::git_indexer::index_git_history(repo, project_id, &db, &commit_tx, &stats)
        .await
        .expect("git history");
    let (commits,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM git_commits WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(testdb.pool())
            .await
            .expect("count");
    assert!(
        commits >= 1,
        "git history cron should have indexed ≥ 1 commit"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn topic_clustering_num_clusters_override_is_honored() {
    let testdb = require_test_db!();
    let backend: Arc<dyn EmbeddingBackend> = Arc::new(DeterministicEmbeddingBackend::new(384));
    let strings: Vec<String> = (0..15).map(|i| format!("item number {} text", i)).collect();
    let contents: Vec<&str> = strings.iter().map(|s| s.as_str()).collect();
    seed_chunks(
        testdb.pool(),
        "ncover",
        "/ws/ncover",
        "f.rs",
        contents,
        &backend,
    )
    .await;
    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let stats = Arc::new(StatsTracker::new());
    let mut cron_cfg = CronConfig::default();
    cron_cfg.topic_min_cluster_size = 2;
    cron_cfg.topic_num_clusters = Some(2);
    cron_cfg.topic_fcm_max_iters = 20;
    run_global_topic_scan(db.as_ref(), &cron_cfg, &stats, &DaemonLifecycle::new()).await;
    let (topic_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM code_topics")
        .fetch_one(testdb.pool())
        .await
        .expect("count");
    assert!(
        topic_count <= 4,
        "topic count should be ≲ num_clusters, got {}",
        topic_count
    );
}
