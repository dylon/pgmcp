//! Real-Postgres correctness oracles for `pgmcp::db::queries::semantic_search`.
//!
//! Where the existing `db_sql_surface_integration.rs` tests proved the
//! cosine operator works on a single-row corpus, these tests pin the
//! actual semantic_search query path:
//!
//! - HNSW recall against an exhaustive linear scan over the same data
//! - Rank order on a hand-pinned 5-chunk corpus where the query's
//!   cosine to each chunk is computable on paper
//! - `SET LOCAL hnsw.ef_search` transaction scoping (no leak across
//!   pool connections)
//! - `language` filter isolation
//! - `project` filter isolation
//!
//! All tests skip cleanly with `SKIPPED:` if no test DB is configured.

use pgmcp::db::queries::{SearchResult, semantic_search};
use pgmcp_testing::fixtures::test_embedding;
use pgmcp_testing::require_test_db;
use sqlx::PgPool;

const D: usize = 384;

/// Insert a project and return its id.
async fn insert_project(pool: &PgPool, name: &str) -> i32 {
    sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind("/ws")
    .bind(format!("/ws/{name}"))
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("insert project")
}

/// Insert a file and return its id.
async fn insert_file(pool: &PgPool, project_id: i32, path: &str, language: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO indexed_files \
         (project_id, path, relative_path, language, size_bytes, content, line_count, modified_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, NOW()) RETURNING id",
    )
    .bind(project_id)
    .bind(format!("/ws/{path}"))
    .bind(path)
    .bind(language)
    .bind(64_i64)
    .bind("body")
    .bind(1_i32)
    .fetch_one(pool)
    .await
    .expect("insert file")
}

/// Insert a chunk with the given embedding. Returns the chunk id.
async fn insert_chunk(pool: &PgPool, file_id: i64, idx: i32, embedding: &[f32]) -> i64 {
    let v = pgvector::Vector::from(embedding.to_vec());
    sqlx::query_scalar(
        "INSERT INTO file_chunks (file_id, chunk_index, content, start_line, end_line, embedding) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(file_id)
    .bind(idx)
    .bind(format!("chunk {idx}"))
    .bind(1_i32)
    .bind(1_i32)
    .bind(v)
    .fetch_one(pool)
    .await
    .expect("insert chunk")
}

/// L2-normalized basis vector with `1.0` at one position. Used to build
/// pinned-cosine fixtures where similarity is a rational number.
fn basis(idx: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; D];
    v[idx] = 1.0;
    v
}

/// Brute-force top-k via SQL with HNSW index disabled. Forces a
/// sequential scan so the result is the *exact* nearest-neighbor set.
async fn brute_force_top_k(pool: &PgPool, query: &[f32], k: i32) -> Vec<i64> {
    let mut tx = pool.begin().await.expect("begin");
    sqlx::query("SET LOCAL enable_indexscan = off")
        .execute(&mut *tx)
        .await
        .expect("disable indexscan");
    sqlx::query("SET LOCAL enable_bitmapscan = off")
        .execute(&mut *tx)
        .await
        .expect("disable bitmapscan");

    let v = pgvector::Vector::from(query.to_vec());
    let rows: Vec<(i64,)> =
        sqlx::query_as("SELECT id FROM file_chunks ORDER BY embedding <=> $1 LIMIT $2")
            .bind(v)
            .bind(k)
            .fetch_all(&mut *tx)
            .await
            .expect("brute scan");
    rows.into_iter().map(|(id,)| id).collect()
}

// ============================================================================
// 1. HNSW recall vs brute-force linear scan
// ============================================================================

#[tokio::test]
async fn hnsw_recall_matches_brute_force_within_recall_floor() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = insert_project(&pool, "recall-test").await;
    let file_id = insert_file(&pool, project_id, "recall.rs", "rust").await;

    // Seed 200 deterministic 384-dim L2-normalized embeddings. The
    // index is built incrementally as we INSERT — this is the same
    // path production uses.
    for i in 0..200 {
        let emb = test_embedding(D, &format!("recall-{i}"));
        insert_chunk(&pool, file_id, i, &emb).await;
    }

    // Query vector picked deterministically too.
    let query = test_embedding(D, "recall-query");
    const K: i32 = 10;

    let hnsw_results: Vec<SearchResult> =
        semantic_search(&pool, &query, K, None, None, /*ef_search*/ 100, false)
            .await
            .expect("hnsw search");
    assert_eq!(hnsw_results.len(), K as usize, "HNSW returned wrong count");

    // Brute-force ground truth.
    let truth: Vec<i64> = brute_force_top_k(&pool, &query, K).await;

    // Map HNSW results back to chunk IDs by their (path, line) keys —
    // which we can't, since SearchResult doesn't expose chunk_id.
    // Instead, intersect by score: a chunk is "in HNSW top-k" iff its
    // similarity equals one of HNSW's reported scores within fp tol.
    // Cleaner: re-fetch chunk_id by joining on the HNSW result's
    // (file path, content) — but content has chunk index. Easiest:
    // compare the *set of dot products* HNSW vs brute force.
    let truth_scores: Vec<f64> = {
        // For each truth chunk_id, compute its true cosine to query
        // and round to the same precision as the SearchResult.score.
        let v = pgvector::Vector::from(query.clone());
        let rows: Vec<(i64, f64)> =
            sqlx::query_as("SELECT id, 1 - (embedding <=> $1) FROM file_chunks WHERE id = ANY($2)")
                .bind(v)
                .bind(&truth)
                .fetch_all(&pool)
                .await
                .expect("truth scores");
        let mut scores: Vec<f64> = rows.into_iter().map(|(_, s)| s).collect();
        scores.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        scores
    };
    let hnsw_scores: Vec<f64> = {
        let mut s: Vec<f64> = hnsw_results.iter().filter_map(|r| r.score).collect();
        s.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        s
    };

    // Recall = (intersection size of top-K score sets) / K. Match by
    // score within 1e-6 tolerance.
    let mut overlap = 0;
    let mut taken = vec![false; truth_scores.len()];
    for h in &hnsw_scores {
        for (j, t) in truth_scores.iter().enumerate() {
            if !taken[j] && (h - t).abs() < 1e-6 {
                taken[j] = true;
                overlap += 1;
                break;
            }
        }
    }
    let recall = overlap as f64 / K as f64;
    assert!(
        recall >= 0.95,
        "HNSW recall = {recall} (overlap {overlap}/{K}); truth {truth_scores:?} hnsw {hnsw_scores:?}"
    );
}

// ============================================================================
// 2. Rank order on a pinned-cosine corpus
// ============================================================================

#[tokio::test]
async fn semantic_search_returns_correct_rank_order_on_pinned_corpus() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = insert_project(&pool, "rank-pinned").await;
    let file_id = insert_file(&pool, project_id, "pin.rs", "rust").await;

    // Query is e0 (basis vector at position 0).
    // Insert 5 chunks whose cosine to query is known:
    //   chunk 0: e0           cosine = 1.0          (perfect match)
    //   chunk 1: e0 + e1 norm cosine = 1/√2 ≈ 0.707
    //   chunk 2: e0 + e2 norm cosine = 1/√2 ≈ 0.707 (tied with chunk 1)
    //   chunk 3: e1           cosine = 0.0          (orthogonal)
    //   chunk 4: -e0          cosine = -1.0         (antipodal)
    let mut e01 = vec![0.0_f32; D];
    e01[0] = 1.0;
    e01[1] = 1.0;
    let n = (2.0_f32).sqrt();
    for x in e01.iter_mut() {
        *x /= n;
    }
    let mut e02 = vec![0.0_f32; D];
    e02[0] = 1.0;
    e02[2] = 1.0;
    for x in e02.iter_mut() {
        *x /= n;
    }
    let mut neg_e0 = basis(0);
    for x in neg_e0.iter_mut() {
        *x = -*x;
    }

    insert_chunk(&pool, file_id, 0, &basis(0)).await;
    insert_chunk(&pool, file_id, 1, &e01).await;
    insert_chunk(&pool, file_id, 2, &e02).await;
    insert_chunk(&pool, file_id, 3, &basis(1)).await;
    insert_chunk(&pool, file_id, 4, &neg_e0).await;

    let query = basis(0);
    let results = semantic_search(&pool, &query, 5, None, None, 100, false)
        .await
        .expect("search");
    assert_eq!(results.len(), 5);

    // Top result must be chunk 0 with score ≈ 1.0.
    let top = &results[0];
    assert!(
        top.chunk_content.contains("chunk 0"),
        "top {:?}",
        top.chunk_content
    );
    assert!(
        (top.score.unwrap() - 1.0).abs() < 1e-5,
        "top score = {:?}",
        top.score
    );

    // Last result must be chunk 4 (antipodal) with score ≈ -1.0.
    let last = &results[4];
    assert!(
        last.chunk_content.contains("chunk 4"),
        "last {:?}",
        last.chunk_content
    );
    assert!(
        (last.score.unwrap() - (-1.0)).abs() < 1e-5,
        "last score = {:?}",
        last.score
    );

    // Chunk 3 (orthogonal, cosine 0) ranks just before the antipodal.
    let fourth = &results[3];
    assert!(fourth.chunk_content.contains("chunk 3"));
    assert!(fourth.score.unwrap().abs() < 1e-5);

    // Chunks 1 and 2 share rank 1/2 (cosine 1/√2). Verify both are in
    // the middle two slots and have the expected score.
    let middle: std::collections::BTreeSet<&str> = [
        results[1].chunk_content.as_str(),
        results[2].chunk_content.as_str(),
    ]
    .into_iter()
    .collect();
    assert!(middle.contains("chunk 1"), "middle {middle:?}");
    assert!(middle.contains("chunk 2"), "middle {middle:?}");
    let one_over_sqrt2 = 1.0_f64 / 2.0_f64.sqrt();
    for r in &results[1..=2] {
        assert!(
            (r.score.unwrap() - one_over_sqrt2).abs() < 1e-5,
            "middle score = {:?}",
            r.score
        );
    }
}

// ============================================================================
// 3. ef_search SET LOCAL transaction scoping
// ============================================================================

#[tokio::test]
async fn ef_search_set_local_does_not_leak_across_pooled_connections() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = insert_project(&pool, "ef-leak").await;
    let file_id = insert_file(&pool, project_id, "ef.rs", "rust").await;
    insert_chunk(&pool, file_id, 0, &basis(0)).await;

    let query = basis(0);

    // Issue first search with a custom ef_search.
    let _ = semantic_search(&pool, &query, 1, None, None, 13, false)
        .await
        .expect("search 1");

    // After the transaction commits, ef_search should NOT persist on
    // the connection. Open a fresh connection (the pool may reuse the
    // same one) and read SHOW hnsw.ef_search.
    //
    // pgvector's ef_search default is 40. If SET LOCAL leaked, we'd see
    // 13 here. SHOW returns the *current* (possibly pgvector-default)
    // value for the session — we accept anything ≠ 13 as "did not leak"
    // to remain robust against pgvector changing its default.
    let leaked: (String,) = sqlx::query_as("SHOW hnsw.ef_search")
        .fetch_one(&pool)
        .await
        .expect("show");
    let value: i32 = leaked.0.parse().expect("parse");
    assert_ne!(
        value, 13,
        "SET LOCAL leaked across pool connection: showed {value}"
    );

    // Issue a second search with a DIFFERENT custom ef_search and
    // verify the same non-leak holds.
    let _ = semantic_search(&pool, &query, 1, None, None, 77, false)
        .await
        .expect("search 2");
    let leaked2: (String,) = sqlx::query_as("SHOW hnsw.ef_search")
        .fetch_one(&pool)
        .await
        .expect("show");
    let value2: i32 = leaked2.0.parse().expect("parse");
    assert_ne!(
        value2, 77,
        "SET LOCAL leaked across pool connection on second call: showed {value2}"
    );
}

// ============================================================================
// 4. Language filter isolation
// ============================================================================

#[tokio::test]
async fn language_filter_isolates_results() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = insert_project(&pool, "lang-iso").await;
    let rs_file = insert_file(&pool, project_id, "x.rs", "rust").await;
    let py_file = insert_file(&pool, project_id, "x.py", "python").await;

    // Identical embeddings — only the language metadata differs.
    let emb = basis(0);
    insert_chunk(&pool, rs_file, 0, &emb).await;
    insert_chunk(&pool, py_file, 0, &emb).await;

    // Filter by rust → only the .rs chunk should appear.
    let rust_only = semantic_search(&pool, &emb, 10, Some("rust"), None, 100, false)
        .await
        .expect("search rust");
    assert_eq!(rust_only.len(), 1, "rust filter should yield 1");
    assert_eq!(rust_only[0].relative_path, "x.rs");
    assert_eq!(rust_only[0].language, "rust");

    // Filter by python → only the .py chunk.
    let python_only = semantic_search(&pool, &emb, 10, Some("python"), None, 100, false)
        .await
        .expect("search python");
    assert_eq!(python_only.len(), 1);
    assert_eq!(python_only[0].relative_path, "x.py");
    assert_eq!(python_only[0].language, "python");

    // No filter → both chunks.
    let unfiltered = semantic_search(&pool, &emb, 10, None, None, 100, false)
        .await
        .expect("search unfiltered");
    assert_eq!(unfiltered.len(), 2, "unfiltered should yield both");
}

// ============================================================================
// 5. Project filter isolation
// ============================================================================

#[tokio::test]
async fn project_filter_isolates_results() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let alpha_id = insert_project(&pool, "proj-alpha").await;
    let beta_id = insert_project(&pool, "proj-beta").await;
    let alpha_file = insert_file(&pool, alpha_id, "a/x.rs", "rust").await;
    let beta_file = insert_file(&pool, beta_id, "b/x.rs", "rust").await;

    let emb = basis(0);
    insert_chunk(&pool, alpha_file, 0, &emb).await;
    insert_chunk(&pool, beta_file, 0, &emb).await;

    let alpha_only = semantic_search(&pool, &emb, 10, None, Some("proj-alpha"), 100, false)
        .await
        .expect("alpha");
    assert_eq!(alpha_only.len(), 1);
    assert_eq!(alpha_only[0].project_name, "proj-alpha");

    let beta_only = semantic_search(&pool, &emb, 10, None, Some("proj-beta"), 100, false)
        .await
        .expect("beta");
    assert_eq!(beta_only.len(), 1);
    assert_eq!(beta_only[0].project_name, "proj-beta");

    let both_filters = semantic_search(
        &pool,
        &emb,
        10,
        Some("rust"),
        Some("proj-alpha"),
        100,
        false,
    )
    .await
    .expect("both filters");
    assert_eq!(both_filters.len(), 1);
    assert_eq!(both_filters[0].project_name, "proj-alpha");
    assert_eq!(both_filters[0].language, "rust");
}
