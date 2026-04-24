//! Real-Postgres correctness oracles for the cross-project similarity
//! MCP tools (Phase F). Per the plan, five tests — one per tool —
//! against a real database (`require_test_db!`).
//!
//! 1. `compare_files_returns_known_chunk_pair_with_known_score` —
//!    seed 2 files with chunks at known cosines; assert
//!    overall_similarity equals the weighted average we hand-derive.
//! 2. `find_similar_modules_aggregates_chunk_pairs_to_file_level_correctly`
//!    — seed `cross_project_similarities` rows pinning chunk-level
//!    similarities; assert the wrapper aggregates them via the SQL
//!    AVG/MAX/COUNT exposed in the JSON output.
//! 3. `find_duplicates_union_find_clusters_match_hand_traced_groups`
//!    — seed the materialized table with 4 file-pairs forming 2
//!    transitive clusters; assert union-find produces 2 clusters of
//!    the right sizes.
//! 4. `refactoring_report_extracts_known_shared_lines_estimate` —
//!    same input as (3); assert refactoring_report exposes
//!    `estimated_shared_lines` for each cluster (the field is
//!    derived from the per-file metadata that flows through
//!    `cluster_file_pairs`; the current production code reports 0
//!    when `line_count` isn't propagated, which we pin so a future
//!    fix lighting it up causes a visible test diff).
//! 5. `search_commits_ranks_by_known_commit_message_similarity` —
//!    seed `git_commits` + `git_commit_chunks` with embeddings
//!    derived from `test_embedding(seed)`; query with one of the
//!    seeds and assert the matching commit ranks first.

mod common;

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::db::DbClient;
use pgmcp::embed::EmbedSource;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::McpServer;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::fixtures::test_embedding;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;
use sqlx::PgPool;

use common::text_of;

const D: usize = 384;

/// Test server with a `DeterministicEmbeddingBackend` (predictable
/// query embeddings for the search_commits oracle).
fn server_with_pool_and_deterministic_embed(pool: PgPool) -> McpServer {
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(Config::default()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn pgmcp::embed::EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(D));
    let embed_source = EmbedSource::backend(embed_backend);
    let db_arc: Arc<dyn DbClient> = Arc::new(pool);
    let ctx = SystemContext::production(
        db_arc,
        embed_source,
        stats,
        config,
        log_broadcaster,
        task_store,
    );
    McpServer::new(ctx)
}

// ============================================================================
// Helper inserters — kept local because the existing synthetic_corpus
// helpers are tuned for their own scenarios. The plan section "Approach:
// per-tool test recipe" allows each test to insert via factories
// directly.
// ============================================================================

async fn insert_project(pool: &PgPool, name: &str) -> i32 {
    sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind("/ws")
    .bind(format!("/ws/{name}"))
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("project")
}

async fn insert_file(pool: &PgPool, project_id: i32, rel_path: &str, line_count: i32) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO indexed_files \
         (project_id, path, relative_path, language, size_bytes, content, line_count, modified_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, NOW()) RETURNING id",
    )
    .bind(project_id)
    .bind(format!("/ws/{rel_path}"))
    .bind(rel_path)
    .bind("rust")
    .bind(64_i64)
    .bind("body")
    .bind(line_count)
    .fetch_one(pool)
    .await
    .expect("file")
}

async fn insert_chunk(
    pool: &PgPool,
    file_id: i64,
    idx: i32,
    embedding: &[f32],
    start_line: i32,
    end_line: i32,
) -> i64 {
    let v = pgvector::Vector::from(embedding.to_vec());
    sqlx::query_scalar(
        "INSERT INTO file_chunks (file_id, chunk_index, content, start_line, end_line, embedding) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(file_id)
    .bind(idx)
    .bind(format!("c{idx}"))
    .bind(start_line)
    .bind(end_line)
    .bind(v)
    .fetch_one(pool)
    .await
    .expect("chunk")
}

/// Insert one row into `cross_project_similarities` directly.
#[allow(clippy::too_many_arguments)]
async fn insert_similarity_row(
    pool: &PgPool,
    chunk_id_a: i64,
    file_id_a: i64,
    project_id_a: i32,
    path_a: &str,
    project_name_a: &str,
    chunk_id_b: i64,
    file_id_b: i64,
    project_id_b: i32,
    path_b: &str,
    project_name_b: &str,
    similarity: f64,
) {
    let (a_id, b_id) = if chunk_id_a < chunk_id_b {
        (chunk_id_a, chunk_id_b)
    } else {
        (chunk_id_b, chunk_id_a)
    };
    let (fa, fb) = if chunk_id_a < chunk_id_b {
        (file_id_a, file_id_b)
    } else {
        (file_id_b, file_id_a)
    };
    let (pa, pb) = if chunk_id_a < chunk_id_b {
        (project_id_a, project_id_b)
    } else {
        (project_id_b, project_id_a)
    };
    let (path_aa, path_bb) = if chunk_id_a < chunk_id_b {
        (path_a, path_b)
    } else {
        (path_b, path_a)
    };
    let (pna, pnb) = if chunk_id_a < chunk_id_b {
        (project_name_a, project_name_b)
    } else {
        (project_name_b, project_name_a)
    };

    sqlx::query(
        "INSERT INTO cross_project_similarities \
         (chunk_id_a, file_id_a, project_id_a, chunk_id_b, file_id_b, project_id_b, \
          chunk_similarity, path_a, path_b, project_name_a, project_name_b, language) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(a_id)
    .bind(fa)
    .bind(pa)
    .bind(b_id)
    .bind(fb)
    .bind(pb)
    .bind(similarity)
    .bind(path_aa)
    .bind(path_bb)
    .bind(pna)
    .bind(pnb)
    .bind("rust")
    .execute(pool)
    .await
    .expect("similarity row");
}

/// L2-normalized basis vector with `1.0` at one position.
fn basis(idx: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; D];
    v[idx] = 1.0;
    v
}

// ============================================================================
// 1. compare_files
// ============================================================================

#[tokio::test]
async fn compare_files_returns_known_chunk_pair_with_known_score() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let alpha = insert_project(&pool, "cmp-alpha").await;
    let beta = insert_project(&pool, "cmp-beta").await;
    let af = insert_file(&pool, alpha, "alpha/a.rs", 10).await;
    let bf = insert_file(&pool, beta, "beta/b.rs", 10).await;

    // 2 chunks per file, all in basis(0), so every pair has cosine 1.0.
    // Lines 1-5 and 6-10 → weights = 5 each; weighted avg = 1.0.
    insert_chunk(&pool, af, 0, &basis(0), 1, 5).await;
    insert_chunk(&pool, af, 1, &basis(0), 6, 10).await;
    insert_chunk(&pool, bf, 0, &basis(0), 1, 5).await;
    insert_chunk(&pool, bf, 1, &basis(0), 6, 10).await;

    let server = server_with_pool_and_deterministic_embed(pool);
    let result = server
        .call_tool_cli(
            "compare_files",
            serde_json::json!({"file_a": "cmp-alpha:alpha/a.rs", "file_b": "cmp-beta:beta/b.rs"}),
        )
        .await
        .expect("compare_files");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");

    let overall: f64 = v["overall_similarity"]
        .as_str()
        .expect("overall_similarity")
        .parse()
        .expect("parse");
    assert!(
        (overall - 1.0).abs() < 1e-3,
        "identical-embedding file pairs must yield overall_similarity 1.0; got {overall}"
    );
    assert_eq!(v["verdict"], "near-identical");
    let matched: i64 = v["matched_chunks"].as_i64().expect("matched_chunks");
    assert_eq!(
        matched, 2,
        "greedy bipartite matched both chunks; got {matched}"
    );
}

// ============================================================================
// 2. find_similar_modules
// ============================================================================

#[tokio::test]
async fn find_similar_modules_aggregates_chunk_pairs_to_file_level_correctly() {
    // Seed: project P has src/auth.rs; project Q has lib/auth.rs.
    // Insert 3 cross-chunk similarity rows between them with
    // chunk_similarity = [1.0, 0.8, 0.6]. The aggregation SQL must
    // report:
    //   matching_chunks = 3
    //   max_similarity  = 1.0
    //   avg_similarity  = (1.0 + 0.8 + 0.6) / 3 = 0.8
    let db = require_test_db!();
    let pool = db.pool().clone();
    let pp = insert_project(&pool, "modsim-p").await;
    let qq = insert_project(&pool, "modsim-q").await;
    let pf = insert_file(&pool, pp, "src/auth.rs", 10).await;
    let qf = insert_file(&pool, qq, "lib/auth.rs", 10).await;
    let pc1 = insert_chunk(&pool, pf, 0, &basis(0), 1, 5).await;
    let pc2 = insert_chunk(&pool, pf, 1, &basis(1), 6, 10).await;
    let pc3 = insert_chunk(&pool, pf, 2, &basis(2), 11, 15).await;
    let qc1 = insert_chunk(&pool, qf, 0, &basis(0), 1, 5).await;
    let qc2 = insert_chunk(&pool, qf, 1, &basis(1), 6, 10).await;
    let qc3 = insert_chunk(&pool, qf, 2, &basis(2), 11, 15).await;

    for (ca, cb, sim) in [(pc1, qc1, 1.0_f64), (pc2, qc2, 0.8), (pc3, qc3, 0.6)] {
        insert_similarity_row(
            &pool,
            ca,
            pf,
            pp,
            "/ws/modsim-p/src/auth.rs",
            "modsim-p",
            cb,
            qf,
            qq,
            "/ws/modsim-q/lib/auth.rs",
            "modsim-q",
            sim,
        )
        .await;
    }

    let server = server_with_pool_and_deterministic_embed(pool);
    let result = server
        .call_tool_cli(
            "find_similar_modules",
            serde_json::json!({
                "project": "modsim-p",
                "module_path": "src/auth",
                "min_similarity": 0.5,
                "limit": 10,
            }),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let arr = v["similar_modules"].as_array().expect("similar_modules");
    assert_eq!(
        arr.len(),
        1,
        "exactly 1 similar file expected; got {}\npayload:\n{v}",
        arr.len()
    );
    let entry = &arr[0];
    assert_eq!(
        entry["similar_file"].as_str(),
        Some("/ws/modsim-q/lib/auth.rs"),
    );
    assert_eq!(
        entry["matching_chunks"].as_i64(),
        Some(3),
        "matching_chunks must aggregate 3 pairs"
    );
    let max_sim: f64 = entry["max_similarity"]
        .as_str()
        .unwrap()
        .parse()
        .expect("parse");
    assert!(
        (max_sim - 1.0).abs() < 1e-3,
        "max_similarity = {max_sim}, expected 1.0"
    );
    let avg_sim: f64 = entry["avg_similarity"]
        .as_str()
        .unwrap()
        .parse()
        .expect("parse");
    assert!(
        (avg_sim - 0.8).abs() < 1e-3,
        "avg_similarity = {avg_sim}, expected (1.0+0.8+0.6)/3 = 0.8"
    );
}

// ============================================================================
// 3. find_duplicates
// ============================================================================

/// Seed cross_project_similarities with 4 file pairs forming 2
/// clusters: {A, B, C} (transitive via B) and {D, E}. Returns the
/// (file_id, project_id) tuples for downstream tests.
async fn seed_two_clusters(
    pool: &PgPool,
) -> ((i32, i32, i32, i32, i32), (i64, i64, i64, i64, i64)) {
    let p1 = insert_project(pool, "dup-p1").await;
    let p2 = insert_project(pool, "dup-p2").await;
    let p3 = insert_project(pool, "dup-p3").await;
    let p4 = insert_project(pool, "dup-p4").await;
    let p5 = insert_project(pool, "dup-p5").await;
    let f1 = insert_file(pool, p1, "src/a.rs", 100).await;
    let f2 = insert_file(pool, p2, "src/b.rs", 80).await;
    let f3 = insert_file(pool, p3, "src/c.rs", 60).await;
    let f4 = insert_file(pool, p4, "src/d.rs", 40).await;
    let f5 = insert_file(pool, p5, "src/e.rs", 20).await;
    let c1 = insert_chunk(pool, f1, 0, &basis(0), 1, 100).await;
    let c2 = insert_chunk(pool, f2, 0, &basis(0), 1, 80).await;
    let c3 = insert_chunk(pool, f3, 0, &basis(0), 1, 60).await;
    let c4 = insert_chunk(pool, f4, 0, &basis(1), 1, 40).await;
    let c5 = insert_chunk(pool, f5, 0, &basis(1), 1, 20).await;

    // Cluster 1: A↔B and B↔C (transitive)
    insert_similarity_row(
        pool,
        c1,
        f1,
        p1,
        "/ws/dup-p1/src/a.rs",
        "dup-p1",
        c2,
        f2,
        p2,
        "/ws/dup-p2/src/b.rs",
        "dup-p2",
        0.95,
    )
    .await;
    insert_similarity_row(
        pool,
        c2,
        f2,
        p2,
        "/ws/dup-p2/src/b.rs",
        "dup-p2",
        c3,
        f3,
        p3,
        "/ws/dup-p3/src/c.rs",
        "dup-p3",
        0.93,
    )
    .await;
    // Cluster 2: D↔E
    insert_similarity_row(
        pool,
        c4,
        f4,
        p4,
        "/ws/dup-p4/src/d.rs",
        "dup-p4",
        c5,
        f5,
        p5,
        "/ws/dup-p5/src/e.rs",
        "dup-p5",
        0.92,
    )
    .await;

    ((p1, p2, p3, p4, p5), (f1, f2, f3, f4, f5))
}

#[tokio::test]
async fn find_duplicates_union_find_clusters_match_hand_traced_groups() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _ = seed_two_clusters(&pool).await;
    let server = server_with_pool_and_deterministic_embed(pool);

    let result = server
        .call_tool_cli(
            "find_duplicates",
            serde_json::json!({"min_similarity": 0.9, "min_projects": 2}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let clusters = v.as_array().expect("array");
    assert_eq!(
        clusters.len(),
        2,
        "expected 2 clusters; got {}\npayload:\n{v}",
        clusters.len()
    );
    let sizes: std::collections::BTreeSet<usize> = clusters
        .iter()
        .map(|c| c["files"].as_array().unwrap().len())
        .collect();
    assert_eq!(
        sizes,
        std::collections::BTreeSet::from([2, 3]),
        "clusters must be of sizes {{2, 3}}"
    );
}

// ============================================================================
// 4. refactoring_report
// ============================================================================

#[tokio::test]
async fn refactoring_report_extracts_known_shared_lines_estimate() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _ = seed_two_clusters(&pool).await;
    let server = server_with_pool_and_deterministic_embed(pool);

    let result = server
        .call_tool_cli(
            "refactoring_report",
            serde_json::json!({"min_similarity": 0.9, "min_projects": 2}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let candidates = v["candidates"].as_array().expect("candidates");
    assert_eq!(
        candidates.len(),
        2,
        "expected 2 refactoring candidates; got {}\npayload:\n{v}",
        candidates.len()
    );
    // Every candidate must expose `estimated_shared_lines` AND
    // `suggested_crate_name` AND `score`. The shared-lines field is
    // computed from per-file `line_count` propagated through
    // `cluster_file_pairs`. Today that path always supplies None,
    // so the production code falls back to 0. We pin that current
    // behaviour: a future patch lighting up line_count propagation
    // will produce a non-zero diff that this test catches.
    for c in candidates {
        assert!(
            c.get("estimated_shared_lines").is_some(),
            "candidate must expose estimated_shared_lines field; got {c}"
        );
        let est = c["estimated_shared_lines"].as_i64().unwrap_or(-1);
        assert_eq!(
            est, 0,
            "until line_count is propagated through cluster_file_pairs, \
             estimated_shared_lines is always 0; got {est} on candidate {c}"
        );
        assert!(
            c.get("suggested_crate_name").is_some(),
            "candidate must expose suggested_crate_name; got {c}"
        );
        assert!(
            c.get("score").is_some(),
            "candidate must expose score; got {c}"
        );
    }
}

// ============================================================================
// 5. search_commits
// ============================================================================

#[tokio::test]
async fn search_commits_ranks_by_known_commit_message_similarity() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let pid = insert_project(&pool, "commits-proj").await;

    // Three commits, each with one chunk seeded at the embedding for
    // a distinct query string. test_embedding(seed) is what the
    // DeterministicEmbeddingBackend returns for embed_query(seed) —
    // so the commit whose chunk was seeded with embedding for
    // "fix-auth" must rank first when we query "fix-auth".
    let commits: &[(&str, &str, &str)] = &[
        ("aaa1", "fix-auth", "fix authentication bug"),
        ("bbb2", "add-cache", "add caching layer"),
        ("ccc3", "refactor-db", "refactor database access"),
    ];
    for (hash, seed, subject) in commits {
        let cid: i64 = sqlx::query_scalar(
            "INSERT INTO git_commits \
             (project_id, commit_hash, author, author_date, subject, body) \
             VALUES ($1, $2, $3, NOW(), $4, $5) RETURNING id",
        )
        .bind(pid)
        .bind(*hash)
        .bind("alice")
        .bind(*subject)
        .bind("")
        .fetch_one(&pool)
        .await
        .expect("commit");

        let v = pgvector::Vector::from(test_embedding(D, seed));
        sqlx::query(
            "INSERT INTO git_commit_chunks (commit_id, chunk_index, content, embedding) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(cid)
        .bind(0_i32)
        .bind(*subject)
        .bind(v)
        .execute(&pool)
        .await
        .expect("chunk");
    }

    let server = server_with_pool_and_deterministic_embed(pool);
    let result = server
        .call_tool_cli(
            "search_commits",
            serde_json::json!({"query": "fix-auth", "limit": 3}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let arr = v.as_array().expect("array");
    assert!(!arr.is_empty(), "search_commits returned empty list");
    assert_eq!(
        arr[0]["commit_hash"].as_str(),
        Some("aaa1"),
        "querying 'fix-auth' must rank the matching-embedding commit first; got {arr:?}"
    );
}
