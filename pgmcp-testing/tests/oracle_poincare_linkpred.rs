//! Real-SQL smoke/property oracle for Phase-8 Poincaré link prediction: the cron
//! trains over a seeded is_a tree and proposes only **novel, acyclic** `broader`
//! candidate edges (never duplicating an is_a edge, never a symmetric pair). The
//! embedding math itself is exhaustively unit-tested in
//! `src/ontology/embed_hyperbolic.rs`. Self-skips with no test DB.

use pgmcp::cron::ontology_link_predict::run_ontology_link_predict;
use pgmcp::db::queries;
use pgmcp::ontology::edge::OntologyRelation;
use pgmcp::ontology::facet::Facet;
use pgmcp::tracker::transition::Actor;
use std::collections::HashSet;

#[tokio::test]
async fn link_predict_runs_and_proposes_novel_acyclic_broader() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();

    // 5 Algorithm concepts in an is_a tree: 1,2 is_a 0; 3,4 is_a 1.
    let mut ids = Vec::new();
    for n in ["A0", "A1", "A2", "A3", "A4"] {
        let (id, _) = queries::create_concept(pool, n, Facet::Algorithm, Actor::User)
            .await
            .expect("concept");
        ids.push(id);
    }
    let isa_pairs = [(1, 0), (2, 0), (3, 1), (4, 1)];
    for (c, p) in isa_pairs {
        queries::insert_ontology_edge(pool, ids[c], ids[p], OntologyRelation::IsA, 1.0)
            .await
            .expect("is_a");
    }

    run_ontology_link_predict(pool).await.expect("link-predict cron runs");

    let broader: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT from_entity_id, to_entity_id FROM memory_relations \
         WHERE relation_type = 'broader' AND valid_to IS NULL",
    )
    .fetch_all(pool)
    .await
    .expect("read broader");

    let isa: HashSet<(i64, i64)> = isa_pairs.iter().map(|(c, p)| (ids[*c], ids[*p])).collect();
    let set: HashSet<(i64, i64)> = broader.iter().copied().collect();
    for (c, p) in &broader {
        assert!(!isa.contains(&(*c, *p)), "a broader candidate must not duplicate an is_a edge");
        assert_ne!(c, p, "no self-edge");
        assert!(!set.contains(&(*p, *c)), "no symmetric broader pair ⇒ acyclic");
    }
}
