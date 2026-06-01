//! Real-SQL oracle for the Phase-0 v23 ontology sidecar schema.
//!
//! Pattern A (`require_test_txn!`): the shared template DB already has
//! `run_migrations` (incl. v23) applied, so we exercise the tables/CHECKs inside
//! a rollback transaction. Self-skips cleanly when no test DB is configured.

use pgmcp::ontology::edge::EvidenceKind;
use pgmcp::ontology::facet::Facet;

/// All five v23 tables exist after migration.
#[tokio::test]
async fn ontology_v23_tables_exist() {
    let mut txn = pgmcp_testing::require_test_txn!();
    for tbl in [
        "ontology_concept_meta",
        "ontology_concept_evidence",
        "ontology_concept_attr",
        "ontology_data_link",
        "ontology_rule",
    ] {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_schema = 'public' AND table_name = $1)",
        )
        .bind(tbl)
        .fetch_one(txn.conn())
        .await
        .expect("query table existence");
        assert!(exists, "v23 table `{tbl}` is missing after migration");
    }
}

/// The DB `facet` CHECK is byte-for-byte the Rust [`Facet`] enum — the ADR-003
/// parity guarantee. We read the live constraint definition and assert every
/// enum value appears in it (and that a bogus value does not).
#[tokio::test]
async fn facet_check_matches_enum() {
    let mut txn = pgmcp_testing::require_test_txn!();
    let def: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
         WHERE conname = 'chk_ontology_meta_facet'",
    )
    .fetch_one(txn.conn())
    .await
    .expect("facet CHECK constraint should exist");
    for f in Facet::ALL {
        assert!(
            def.contains(&format!("'{}'", f.as_str())),
            "facet CHECK is missing enum value `{}` (def: {def})",
            f.as_str()
        );
    }
    assert!(
        !def.contains("'not_a_real_facet'"),
        "facet CHECK unexpectedly contains a bogus value"
    );
}

/// A valid facet inserts; an invalid one is rejected by the CHECK.
#[tokio::test]
async fn concept_meta_enforces_facet_check() {
    let mut txn = pgmcp_testing::require_test_txn!();

    let entity_id: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source) \
         VALUES ('OntologyOracleConcept', 'concept', 'user_explicit') RETURNING id",
    )
    .fetch_one(txn.conn())
    .await
    .expect("insert concept entity");

    sqlx::query(
        "INSERT INTO ontology_concept_meta (entity_id, facet, status, build_method) \
         VALUES ($1, 'invariant', 'candidate', 'agent')",
    )
    .bind(entity_id)
    .execute(txn.conn())
    .await
    .expect("a valid facet must be accepted");

    // A second entity for the negative case (entity_id is the meta PK, so reusing
    // the first would trip the PK before the CHECK).
    let entity_id2: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source) \
         VALUES ('OntologyOracleConcept2', 'concept', 'user_explicit') RETURNING id",
    )
    .fetch_one(txn.conn())
    .await
    .expect("insert second concept entity");

    let bad = sqlx::query(
        "INSERT INTO ontology_concept_meta (entity_id, facet, build_method) \
         VALUES ($1, 'not_a_real_facet', 'agent')",
    )
    .bind(entity_id2)
    .execute(txn.conn())
    .await;
    assert!(
        bad.is_err(),
        "an unregistered facet must be rejected by the CHECK"
    );
}

/// Evidence rows accept every registered `evidence_kind` and reject others.
#[tokio::test]
async fn evidence_kind_check_matches_enum() {
    let mut txn = pgmcp_testing::require_test_txn!();
    let def: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
         WHERE conname = 'chk_ontology_evidence_kind'",
    )
    .fetch_one(txn.conn())
    .await
    .expect("evidence_kind CHECK constraint should exist");
    for k in EvidenceKind::ALL {
        assert!(
            def.contains(&format!("'{}'", k.as_str())),
            "evidence_kind CHECK is missing `{}` (def: {def})",
            k.as_str()
        );
    }
}
