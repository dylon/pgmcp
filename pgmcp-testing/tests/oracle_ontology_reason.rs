//! Real-SQL oracle for Phase-9 deductive reasoning + export: is_a cycle
//! detection, the invariant-must-anchor constraint, the transitive is_a closure,
//! and the Prolog export round-trip. (Export string-shaping is also unit-tested
//! in src/ontology/export.rs.) Self-skips with no test DB.

use pgmcp::db::queries;
use pgmcp::ontology::edge::OntologyRelation;
use pgmcp::ontology::facet::Facet;
use pgmcp::ontology::{export, reason};
use pgmcp::tracker::transition::Actor;
use sqlx::PgPool;
use std::collections::HashSet;

async fn concept(pool: &PgPool, name: &str, facet: Facet) -> i64 {
    queries::create_concept(pool, name, facet, Actor::User)
        .await
        .expect("concept")
        .0
}

#[tokio::test]
async fn detects_is_a_cycle() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();
    let a = concept(pool, "CycA", Facet::Component).await;
    let b = concept(pool, "CycB", Facet::Component).await;
    queries::insert_ontology_edge(pool, a, b, OntologyRelation::IsA, 1.0).await.unwrap();
    queries::insert_ontology_edge(pool, b, a, OntologyRelation::IsA, 1.0).await.unwrap();

    let violations = reason::check_constraints(pool).await.expect("check");
    assert!(
        violations.iter().any(|v| v.kind == "is_a_cycle"),
        "an is_a cycle must be flagged, got {violations:?}"
    );
}

#[tokio::test]
async fn flags_unanchored_invariant() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();
    // file = None ⇒ no anchor ⇒ a constraint violation.
    queries::agent_assert_invariant(pool, "FloatingRule", "must hold somewhere", "r", None)
        .await
        .unwrap();

    let violations = reason::check_constraints(pool).await.expect("check");
    assert!(
        violations
            .iter()
            .any(|v| v.kind == "unanchored_invariant" && v.detail.contains("FloatingRule")),
        "an unanchored invariant must be flagged, got {violations:?}"
    );
}

#[tokio::test]
async fn transitive_is_a_closure() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();
    let a = concept(pool, "AncA", Facet::Algorithm).await;
    let b = concept(pool, "AncB", Facet::Algorithm).await;
    let c = concept(pool, "AncC", Facet::Algorithm).await;
    queries::insert_ontology_edge(pool, a, b, OntologyRelation::IsA, 1.0).await.unwrap();
    queries::insert_ontology_edge(pool, b, c, OntologyRelation::IsA, 1.0).await.unwrap();

    let ancestors = queries::concept_ancestors(pool, a).await.expect("ancestors");
    let ids: HashSet<i64> = ancestors.iter().map(|(id, _)| *id).collect();
    assert!(ids.contains(&b), "direct parent");
    assert!(ids.contains(&c), "transitive grandparent (deductive closure)");
}

#[tokio::test]
async fn export_round_trips_to_prolog() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();
    let parent = concept(pool, "ExpParser", Facet::Component).await;
    let child = concept(pool, "ExpRecursiveDescent", Facet::Component).await;
    queries::insert_ontology_edge(pool, child, parent, OntologyRelation::IsA, 1.0).await.unwrap();

    let concepts = queries::export_concepts(pool).await.expect("concepts");
    let edges = queries::export_edges(pool).await.expect("edges");
    assert!(concepts.iter().any(|(_, n, f, _)| n == "ExpParser" && f == "component"));
    assert!(edges.iter().any(|(from, to, r)| *from == child && *to == parent && r == "is_a"));

    let prolog = export::to_prolog(&concepts, &edges);
    assert!(prolog.contains(&format!("is_a({child}, {parent}).")));
    assert!(prolog.contains("'ExpParser'"));
}
