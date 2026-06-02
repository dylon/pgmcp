//! Real-SQL oracle for Phase-5 EDC canonicalization: same-facet concepts whose
//! observation embeddings are near-identical get a `broader` link; a divergent
//! concept does not. Embeddings are seeded directly as pgvector literals.
//! Self-skips with no test DB.

use pgmcp::ontology::facet::Facet;
use pgmcp::ontology::hierarchy::build_broader_edges;
use sqlx::PgPool;

/// A 1024-d pgvector literal that is all-zero except a single hot dimension.
/// Two concepts sharing a hot dim have cosine 1.0; different hot dims → 0.0.
fn vec_lit(hot: usize) -> String {
    let mut parts = vec!["0"; 1024];
    parts[hot] = "1";
    format!("[{}]", parts.join(","))
}

async fn concept_with_embedding(pool: &PgPool, name: &str, hot: usize) -> i64 {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source) \
         VALUES ($1, 'concept', 'user_explicit') RETURNING id",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("insert concept");
    sqlx::query(
        "INSERT INTO ontology_concept_meta (entity_id, facet, build_method) \
         VALUES ($1, 'algorithm', 'topic_seed')",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("insert meta");
    sqlx::query(
        "INSERT INTO memory_observations (entity_id, content, content_sha256, source, embedding) \
         VALUES ($1, $2, $3, 'auto_index', $4::vector)",
    )
    .bind(id)
    .bind(name)
    .bind(format!("sha-{id}"))
    .bind(vec_lit(hot))
    .execute(pool)
    .await
    .expect("insert embedded observation");
    id
}

#[tokio::test]
async fn edc_links_near_duplicates_only() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();

    // c1, c2 share hot dim 0 (cosine 1.0); c3 is orthogonal (cosine 0.0).
    let c1 = concept_with_embedding(pool, "error handling", 0).await;
    let c2 = concept_with_embedding(pool, "error-handling", 0).await;
    let c3 = concept_with_embedding(pool, "matrix multiply", 7).await;

    let n = build_broader_edges(pool, Facet::Algorithm, 0.92)
        .await
        .expect("build broader edges");
    assert_eq!(
        n, 1,
        "exactly one broader link between the two near-identical concepts"
    );

    let edges: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT from_entity_id, to_entity_id FROM memory_relations \
         WHERE relation_type = 'broader' AND valid_to IS NULL",
    )
    .fetch_all(pool)
    .await
    .expect("read broader edges");
    assert_eq!(edges.len(), 1);
    let (from, to) = edges[0];
    // The variant (higher id) points to the canonical (lower id).
    assert_eq!((from, to), (c1.max(c2), c1.min(c2)));
    assert!(
        from != c3 && to != c3,
        "the divergent concept must not be canonicalized"
    );

    // Idempotent.
    let n2 = build_broader_edges(pool, Facet::Algorithm, 0.92)
        .await
        .expect("rebuild");
    assert_eq!(n2, 0, "re-run inserts no new broader edges");
}
