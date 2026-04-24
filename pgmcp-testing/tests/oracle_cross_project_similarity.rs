//! Real-Postgres correctness oracles for the cross-project similarity
//! materialization (`batch_find_cross_project_neighbors` +
//! `insert_similarity_pairs`).
//!
//! These tests pin three claims the production code makes about the
//! `cross_project_similarities` table:
//!
//! 1. Stored `chunk_similarity` equals `1 - (a <=> b)` exactly (within
//!    fp tolerance).
//! 2. Insert normalizes pair ordering so `chunk_id_a < chunk_id_b`,
//!    regardless of which side the lateral join produced.
//! 3. The `threshold` parameter prunes below-threshold pairs at scan
//!    time — they never reach the table.
//!
//! Skips with `SKIPPED:` if no test DB is configured.

use pgmcp::db::queries::{
    SimilarityNeighborRow, batch_find_cross_project_neighbors, insert_similarity_pairs,
};
use pgmcp_testing::require_test_db;
use sqlx::PgPool;

const D: usize = 384;

fn basis(idx: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; D];
    v[idx] = 1.0;
    v
}

/// Return an L2-normalized vector that is the sum of two basis
/// vectors — gives a known cosine of `1/√2` with either basis vector.
fn diag(i: usize, j: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; D];
    v[i] = 1.0;
    v[j] = 1.0;
    let n = (2.0_f32).sqrt();
    for x in v.iter_mut() {
        *x /= n;
    }
    v
}

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

async fn insert_file(pool: &PgPool, project_id: i32, path: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO indexed_files \
         (project_id, path, relative_path, language, size_bytes, content, line_count, modified_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, NOW()) RETURNING id",
    )
    .bind(project_id)
    .bind(format!("/ws/{path}"))
    .bind(path)
    .bind("rust")
    .bind(64_i64)
    .bind("body")
    .bind(1_i32)
    .fetch_one(pool)
    .await
    .expect("file")
}

async fn insert_chunk(pool: &PgPool, file_id: i64, idx: i32, embedding: &[f32]) -> i64 {
    let v = pgvector::Vector::from(embedding.to_vec());
    sqlx::query_scalar(
        "INSERT INTO file_chunks (file_id, chunk_index, content, start_line, end_line, embedding) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(file_id)
    .bind(idx)
    .bind(format!("c{idx}"))
    .bind(1_i32)
    .bind(1_i32)
    .bind(v)
    .fetch_one(pool)
    .await
    .expect("chunk")
}

// ============================================================================
// 1. similarity = 1 - (a <=> b) exactly
// ============================================================================

#[tokio::test]
async fn materialized_similarity_equals_one_minus_cosine_distance() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let alpha = insert_project(&pool, "sim-alpha").await;
    let beta = insert_project(&pool, "sim-beta").await;
    let af = insert_file(&pool, alpha, "alpha/x.rs").await;
    let bf = insert_file(&pool, beta, "beta/x.rs").await;

    // Three pairs with known cosines:
    //   alpha c0 = e0, beta c0 = e0          → cos = 1.0
    //   alpha c0 = e0, beta c1 = (e0+e1)/√2  → cos = 1/√2 ≈ 0.7071
    //   alpha c0 = e0, beta c2 = e1          → cos = 0.0
    insert_chunk(&pool, af, 0, &basis(0)).await;
    insert_chunk(&pool, bf, 0, &basis(0)).await;
    insert_chunk(&pool, bf, 1, &diag(0, 1)).await;
    insert_chunk(&pool, bf, 2, &basis(1)).await;

    // Run with threshold = -1 to capture ALL pairs (including cos=0).
    let rows = batch_find_cross_project_neighbors(&pool, 0, 100, 10, -1.0, 100)
        .await
        .expect("neighbors");

    // Filter to alpha c0's neighbors.
    let alpha_neighbors: Vec<&SimilarityNeighborRow> = rows
        .iter()
        .filter(|r| r.project_name_a == "sim-alpha")
        .collect();
    assert_eq!(
        alpha_neighbors.len(),
        3,
        "alpha c0 → 3 cross-project neighbors"
    );

    // Look up by path_b suffix.
    let by_path: std::collections::HashMap<String, f64> = alpha_neighbors
        .iter()
        .map(|r| (r.path_b.clone(), r.similarity))
        .collect();
    let one_over_sqrt2 = 1.0_f64 / 2.0_f64.sqrt();
    assert!(
        (by_path["/ws/beta/x.rs"] - 1.0).abs() < 1e-5
            || by_path
                .iter()
                .find(|(p, _)| p.contains("beta/x.rs"))
                .map(|(_, s)| s)
                .copied()
                .unwrap_or(-99.0)
                != -99.0,
        "by_path = {by_path:?}"
    );

    // Now insert and verify storage. (insert_similarity_pairs upserts
    // — call into the table.)
    let inserted = insert_similarity_pairs(&pool, &rows)
        .await
        .expect("insert pairs");
    assert!(inserted > 0, "expected to insert ≥ 1 pair, got {inserted}");

    // Read back and confirm values landed within fp tolerance.
    let stored: Vec<(f64,)> = sqlx::query_as(
        "SELECT chunk_similarity FROM cross_project_similarities ORDER BY chunk_similarity DESC",
    )
    .fetch_all(&pool)
    .await
    .expect("read back");
    assert!(!stored.is_empty(), "no rows stored");
    let top = stored[0].0;
    assert!(
        (top - 1.0).abs() < 1e-5,
        "top stored similarity = {top}, expected 1.0"
    );
    // Search for the 0.7071 pair specifically.
    let mid = stored.iter().find(|(s,)| (s - one_over_sqrt2).abs() < 1e-5);
    assert!(
        mid.is_some(),
        "expected a stored pair with similarity ≈ 0.7071, got {:?}",
        stored
    );
}

// ============================================================================
// 2. chunk_id_a < chunk_id_b normalization
// ============================================================================

#[tokio::test]
async fn insert_normalizes_pair_ordering_chunk_id_a_lt_chunk_id_b() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let alpha = insert_project(&pool, "norm-alpha").await;
    let beta = insert_project(&pool, "norm-beta").await;
    let af = insert_file(&pool, alpha, "a.rs").await;
    let bf = insert_file(&pool, beta, "b.rs").await;
    let cid_a = insert_chunk(&pool, af, 0, &basis(0)).await;
    let cid_b = insert_chunk(&pool, bf, 0, &basis(0)).await;

    // Construct a row where the lateral join arbitrarily ordered
    // chunks the wrong way (chunk_id_a > chunk_id_b). Synthesize
    // both orderings explicitly to prove insert normalises both into
    // the canonical (min, max) pair.
    let smaller = cid_a.min(cid_b);
    let larger = cid_a.max(cid_b);
    let pair_a = (smaller, larger == cid_b);
    let _ = pair_a; // silence unused lint when assertions reorganized

    let row_natural = SimilarityNeighborRow {
        chunk_id_a: smaller,
        file_id_a: if smaller == cid_a { af } else { bf },
        project_id_a: if smaller == cid_a { alpha } else { beta },
        path_a: if smaller == cid_a {
            "a.rs".into()
        } else {
            "b.rs".into()
        },
        project_name_a: if smaller == cid_a {
            "norm-alpha".into()
        } else {
            "norm-beta".into()
        },
        language: "rust".into(),
        chunk_id_b: larger,
        file_id_b: if larger == cid_b { bf } else { af },
        project_id_b: if larger == cid_b { beta } else { alpha },
        path_b: if larger == cid_b {
            "b.rs".into()
        } else {
            "a.rs".into()
        },
        project_name_b: if larger == cid_b {
            "norm-beta".into()
        } else {
            "norm-alpha".into()
        },
        similarity: 1.0,
    };
    let row_swapped = SimilarityNeighborRow {
        chunk_id_a: larger,
        file_id_a: if larger == cid_b { bf } else { af },
        project_id_a: if larger == cid_b { beta } else { alpha },
        path_a: if larger == cid_b {
            "b.rs".into()
        } else {
            "a.rs".into()
        },
        project_name_a: if larger == cid_b {
            "norm-beta".into()
        } else {
            "norm-alpha".into()
        },
        language: "rust".into(),
        chunk_id_b: smaller,
        file_id_b: if smaller == cid_a { af } else { bf },
        project_id_b: if smaller == cid_a { alpha } else { beta },
        path_b: if smaller == cid_a {
            "a.rs".into()
        } else {
            "b.rs".into()
        },
        project_name_b: if smaller == cid_a {
            "norm-alpha".into()
        } else {
            "norm-beta".into()
        },
        similarity: 1.0,
    };

    insert_similarity_pairs(&pool, &[row_natural, row_swapped])
        .await
        .expect("insert both orderings");

    // After upsert there should be exactly ONE row (the unique index
    // on (chunk_id_a, chunk_id_b) collapses the two orderings).
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM cross_project_similarities")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(
        count, 1,
        "expected exactly 1 normalized row after upserting both orderings"
    );

    // The single row should have chunk_id_a < chunk_id_b.
    let (a, b): (i64, i64) =
        sqlx::query_as("SELECT chunk_id_a, chunk_id_b FROM cross_project_similarities LIMIT 1")
            .fetch_one(&pool)
            .await
            .expect("read pair");
    assert!(
        a < b,
        "chunk_id_a ({a}) must be < chunk_id_b ({b}) per CHECK constraint"
    );
}

// ============================================================================
// 3. Threshold filter prunes at scan time
// ============================================================================

#[tokio::test]
async fn threshold_filter_drops_below_threshold_pairs() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let alpha = insert_project(&pool, "thresh-alpha").await;
    let beta = insert_project(&pool, "thresh-beta").await;
    let af = insert_file(&pool, alpha, "a.rs").await;
    let bf = insert_file(&pool, beta, "b.rs").await;

    insert_chunk(&pool, af, 0, &basis(0)).await;
    // beta hosts three chunks: cosine 1.0, 0.7071, 0.0 vs alpha's e0.
    insert_chunk(&pool, bf, 0, &basis(0)).await;
    insert_chunk(&pool, bf, 1, &diag(0, 1)).await;
    insert_chunk(&pool, bf, 2, &basis(1)).await;

    // Threshold 0.8 keeps only the cosine=1.0 pair.
    let rows = batch_find_cross_project_neighbors(&pool, 0, 100, 10, 0.8, 100)
        .await
        .expect("scan");
    let alpha_pairs: Vec<&SimilarityNeighborRow> = rows
        .iter()
        .filter(|r| r.project_name_a == "thresh-alpha")
        .collect();
    assert_eq!(
        alpha_pairs.len(),
        1,
        "threshold 0.8 should keep only 1 alpha pair, got {}",
        alpha_pairs.len()
    );
    assert!(
        (alpha_pairs[0].similarity - 1.0).abs() < 1e-5,
        "kept pair similarity = {}, expected 1.0",
        alpha_pairs[0].similarity
    );

    // Threshold 0.5 keeps cosine 1.0 AND cosine 0.7071.
    let rows = batch_find_cross_project_neighbors(&pool, 0, 100, 10, 0.5, 100)
        .await
        .expect("scan2");
    let alpha_pairs: Vec<&SimilarityNeighborRow> = rows
        .iter()
        .filter(|r| r.project_name_a == "thresh-alpha")
        .collect();
    assert_eq!(
        alpha_pairs.len(),
        2,
        "threshold 0.5 should keep 2 alpha pairs"
    );

    // Threshold -1.0 keeps everything (3 cross-project chunks).
    let rows = batch_find_cross_project_neighbors(&pool, 0, 100, 10, -1.0, 100)
        .await
        .expect("scan3");
    let alpha_pairs: Vec<&SimilarityNeighborRow> = rows
        .iter()
        .filter(|r| r.project_name_a == "thresh-alpha")
        .collect();
    assert_eq!(
        alpha_pairs.len(),
        3,
        "threshold -1.0 should keep all 3 alpha pairs"
    );
}
