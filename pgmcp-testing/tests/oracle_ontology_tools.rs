//! Real-SQL oracle for the Phase-6 ontology tool query layer: agent-asserted
//! invariants are candidate-only (trust boundary), concept creation + facet-
//! filtered search, and hierarchy-edge/ resolve helpers. The MCP handler/body
//! layer is a thin compile-checked wrapper over these. Self-skips with no DB.

use pgmcp::db::queries;
use pgmcp::ontology::edge::OntologyRelation;
use pgmcp::ontology::facet::Facet;
use pgmcp::tracker::transition::Actor;

#[tokio::test]
async fn agent_asserted_invariant_is_candidate_only() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();

    let eid = queries::agent_assert_invariant(
        pool,
        "AmbiguityPropagation",
        "ambiguity must propagate end-to-end until evidence rejects it",
        "mettail-rust parser pipeline",
        None,
    )
    .await
    .expect("assert invariant");

    let meta = queries::get_concept_meta(pool, eid)
        .await
        .expect("meta")
        .expect("exists");
    assert_eq!(meta.facet, "invariant");
    assert_eq!(
        meta.status, "candidate",
        "TRUST BOUNDARY: an agent-authored invariant must be candidate, never canonical"
    );
    let ev = queries::list_concept_evidence(pool, eid).await.expect("evidence");
    assert!(
        ev.iter().any(|e| e.evidence_kind == "agent"),
        "agent-kind evidence recorded"
    );
}

#[tokio::test]
async fn create_concept_and_facet_filtered_search() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();

    let (cid, created) =
        queries::create_concept(pool, "Formal Verification Systems", Facet::Collection, Actor::Agent)
            .await
            .expect("create");
    assert!(created);

    let hits = queries::search_concepts_by_name(pool, "Formal Verification", None, 10)
        .await
        .expect("search");
    assert!(hits.iter().any(|h| h.entity_id == cid));

    // Facet filter: matches Collection, not Algorithm.
    let in_collection =
        queries::search_concepts_by_name(pool, "Formal", Some(Facet::Collection), 10)
            .await
            .expect("search facet");
    assert!(in_collection.iter().any(|h| h.entity_id == cid));
    let in_algorithm =
        queries::search_concepts_by_name(pool, "Formal", Some(Facet::Algorithm), 10)
            .await
            .expect("search wrong facet");
    assert!(!in_algorithm.iter().any(|h| h.entity_id == cid));
}

#[tokio::test]
async fn hierarchy_edges_and_resolve() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();

    let (parent, _) = queries::create_concept(pool, "Parser", Facet::Component, Actor::User)
        .await
        .expect("parent");
    let (child, _) =
        queries::create_concept(pool, "RecursiveDescentParser", Facet::Component, Actor::User)
            .await
            .expect("child");
    queries::insert_ontology_edge(pool, child, parent, OntologyRelation::IsA, 1.0)
        .await
        .expect("link");

    let edges = queries::concept_hierarchy_edges(pool, Facet::Component)
        .await
        .expect("edges");
    assert!(
        edges
            .iter()
            .any(|e| e.child_id == child && e.parent_id == parent && e.relation == "is_a"),
        "is_a edge with names surfaces in the tree payload"
    );

    assert_eq!(queries::resolve_concept(pool, "Parser").await.unwrap(), Some(parent));
    assert_eq!(
        queries::resolve_concept(pool, &parent.to_string()).await.unwrap(),
        Some(parent)
    );
    assert_eq!(queries::resolve_concept(pool, "NoSuchConcept").await.unwrap(), None);
}
