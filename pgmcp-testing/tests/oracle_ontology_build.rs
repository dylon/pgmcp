//! Real-SQL oracle for Phase-4 hierarchy construction: the FCA `is_a` Hasse
//! cover persists to `memory_relations`, excludes transitive shortcuts, and is
//! idempotent. (The cover math itself is exhaustively unit-tested in
//! `src/ontology/fca.rs`.) Self-skips with no test DB.

use pgmcp::ontology::fca::ConceptAttrs;
use pgmcp::ontology::hierarchy::build_isa_from_attrs;
use sqlx::PgPool;

async fn concept(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source) \
         VALUES ($1, 'concept', 'user_explicit') RETURNING id",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("insert concept")
}

#[tokio::test]
async fn isa_cover_persists_excludes_shortcuts_and_is_idempotent() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();

    // R{} ⊂ M{1} ⊂ { S1{1,2}, S2{1,3} }.
    let r = concept(pool, "FcaRoot").await;
    let m = concept(pool, "FcaMid").await;
    let s1 = concept(pool, "FcaSpec1").await;
    let s2 = concept(pool, "FcaSpec2").await;
    let concepts = vec![
        ConceptAttrs::new(r, Vec::<u32>::new()),
        ConceptAttrs::new(m, [1u32]),
        ConceptAttrs::new(s1, [1u32, 2]),
        ConceptAttrs::new(s2, [1u32, 3]),
    ];

    let n = build_isa_from_attrs(pool, &concepts)
        .await
        .expect("build is_a");
    assert_eq!(n, 3, "expected exactly M→R, S1→M, S2→M");

    let edges: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT from_entity_id, to_entity_id FROM memory_relations \
         WHERE relation_type = 'is_a' AND valid_to IS NULL",
    )
    .fetch_all(pool)
    .await
    .expect("read is_a edges");
    assert!(edges.contains(&(m, r)), "M is_a R");
    assert!(edges.contains(&(s1, m)), "S1 is_a M");
    assert!(edges.contains(&(s2, m)), "S2 is_a M");
    assert!(!edges.contains(&(s1, r)), "transitive shortcut S1→R must be excluded");
    assert!(!edges.contains(&(s2, r)), "transitive shortcut S2→R must be excluded");
    assert!(!edges.iter().any(|(a, b)| a == b), "no self-edges");
    assert!(!edges.contains(&(r, m)), "no reverse edge");

    // Idempotent: a second build inserts nothing new.
    let n2 = build_isa_from_attrs(pool, &concepts)
        .await
        .expect("rebuild is_a");
    assert_eq!(n2, 0, "re-run must insert no new edges");
}
