//! Real-SQL oracle for the ontology trie accelerators (ADR-012):
//!
//!  * **Accel A** — the persistent concept fuzzy/prefix index: `rebuild_concepts`
//!    materializes the concept trie from PG and `FuzzyIndex::{query,prefix}` give
//!    typo-tolerant + prefix concept lookup, each value carrying facet/id.
//!  * **Accel C (capability)** — `search_concepts_by_name` also matches a concept's
//!    invariant **body** (`constraint_text`), so an invariant surfaces by a word in
//!    its constraint sentence, not just its name.
//!  * **Accel B (capability)** — `concept_descendants` returns a concept's bounded
//!    subtree, correct over the DAG (a recursive closure; depth-bounded).
//!
//! Self-skips with no test DB.

use pgmcp::db::queries;
use pgmcp::fuzzy::persistent_artrie::FuzzyIndex;
use pgmcp::fuzzy::sync::rebuild_concepts;
use pgmcp::fuzzy::values::ConceptValue;
use pgmcp::ontology::edge::OntologyRelation;
use pgmcp::ontology::facet::Facet;
use pgmcp::tracker::transition::Actor;
use tempfile::tempdir;

/// Accel A — concept trie rebuilt from PG answers typo-tolerant + prefix lookups.
#[tokio::test]
async fn concept_trie_fuzzy_and_prefix_lookup() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();

    queries::create_concept(pool, "Concurrency Control", Facet::Concurrency, Actor::User)
        .await
        .expect("seed c1");
    queries::create_concept(pool, "Deadlock Avoidance", Facet::Concurrency, Actor::User)
        .await
        .expect("seed c2");

    // Materialize the concept trie from PG into a temp file (mirrors fuzzy-sync).
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("concepts.artrie");
    let (idx, _) = FuzzyIndex::<ConceptValue>::open_or_create(&path).expect("open trie");
    let n = rebuild_concepts(pool, &idx)
        .await
        .expect("rebuild concepts");
    assert!(n >= 2, "rebuilt at least the two seeded concepts (got {n})");

    // Fuzzy: a one-deletion typo of "Concurrency Control" still matches, and the
    // value carries the concept's facet (single-seek reconstruction).
    let hits = idx.query("Concurency Control", 2);
    assert!(
        hits.iter()
            .any(|(name, _, _)| name == "Concurrency Control"),
        "fuzzy query finds the concept despite a typo"
    );
    assert!(
        hits.iter().any(|(_, _, v)| v.facet == "concurrency"),
        "the trie value carries the concept facet"
    );

    // Prefix: "Deadlock" returns "Deadlock Avoidance"; a non-matching prefix is empty.
    let pre = idx.prefix("Deadlock", 10);
    assert!(
        pre.iter().any(|(name, _)| name == "Deadlock Avoidance"),
        "prefix query finds the concept"
    );
    assert!(
        idx.prefix("Zzz No Such Prefix", 10).is_empty(),
        "a non-matching prefix yields nothing"
    );
}

/// Accel C — search matches an invariant by a word in its body, not just its name.
#[tokio::test]
async fn search_matches_invariant_body() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();

    // The name lacks "ambiguity"; the constraint body carries it.
    queries::agent_assert_invariant(
        pool,
        "PropagateUncertainty",
        "ambiguity must propagate end-to-end until evidence rejects it",
        "mettail-rust parser pipeline",
        None,
    )
    .await
    .expect("assert invariant");

    let by_body = queries::search_concepts_by_name(pool, "ambiguity", None, 30)
        .await
        .expect("body search");
    assert!(
        by_body.iter().any(|h| h.name == "PropagateUncertainty"),
        "a query in the constraint body surfaces the invariant"
    );

    // And the name leg still works.
    let by_name = queries::search_concepts_by_name(pool, "PropagateUncert", None, 30)
        .await
        .expect("name search");
    assert!(by_name.iter().any(|h| h.name == "PropagateUncertainty"));
}

/// Accel B (capability) — bounded subtree descendants, correct over the DAG.
#[tokio::test]
async fn concept_descendants_returns_bounded_subtree() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();

    let (root, _) = queries::create_concept(pool, "Tree", Facet::DataStructure, Actor::User)
        .await
        .expect("root");
    let (a, _) = queries::create_concept(pool, "BalancedTree", Facet::DataStructure, Actor::User)
        .await
        .expect("a");
    let (b, _) = queries::create_concept(pool, "RedBlackTree", Facet::DataStructure, Actor::User)
        .await
        .expect("b");
    // b is_a a is_a root.
    queries::insert_ontology_edge(pool, a, root, OntologyRelation::IsA, 1.0)
        .await
        .expect("a is_a root");
    queries::insert_ontology_edge(pool, b, a, OntologyRelation::IsA, 1.0)
        .await
        .expect("b is_a a");

    // Full subtree: both the direct (a) and transitive (b) descendants appear.
    let edges = queries::concept_descendants(pool, root, 5)
        .await
        .expect("descendants");
    assert!(
        edges.iter().any(|e| e.child_id == a && e.parent_id == root),
        "direct descendant a under root"
    );
    assert!(
        edges.iter().any(|e| e.child_id == b && e.parent_id == a),
        "transitive descendant b under a"
    );

    // Depth bound: depth=1 returns only the direct child, never the depth-2 node.
    let shallow = queries::concept_descendants(pool, root, 1)
        .await
        .expect("shallow descendants");
    assert!(
        shallow.iter().all(|e| e.parent_id == root),
        "depth=1 yields only direct children"
    );
    assert!(
        !shallow.iter().any(|e| e.child_id == b),
        "the depth-2 node is excluded at depth=1"
    );
}
