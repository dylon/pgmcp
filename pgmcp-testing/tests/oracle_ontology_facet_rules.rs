//! Real-SQL oracle for Phase-1 facet metadata: the status chokepoint (agents
//! cannot self-curate), curation-safe re-classification, and the
//! `invariants_for_file` surfacing query. Uses `require_test_db!` (Pattern B) so
//! the `&PgPool`-taking query functions can be exercised directly. Self-skips
//! when no test DB is configured.

use pgmcp::db::queries::{self, SetStatusError};
use pgmcp::ontology::facet::{ConceptStatus, Facet};
use pgmcp::tracker::transition::Actor;
use sqlx::PgPool;

async fn insert_concept(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source) \
         VALUES ($1, 'concept', 'user_explicit') RETURNING id",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("insert concept entity")
}

/// An agent may not move a concept to a curator-only status; a user can.
#[tokio::test]
async fn status_chokepoint_blocks_agent_curation() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();
    let entity_id = insert_concept(pool, "ChokepointConcept").await;
    queries::upsert_concept_meta(pool, entity_id, Facet::Invariant, "agent", None)
        .await
        .expect("upsert meta");

    let blocked = queries::set_concept_status(pool, entity_id, ConceptStatus::Canonical, Actor::Agent)
        .await;
    assert!(
        matches!(blocked, Err(SetStatusError::AgentCannotCurate)),
        "agent must be refused a curator-only status, got {blocked:?}"
    );

    queries::set_concept_status(pool, entity_id, ConceptStatus::Canonical, Actor::User)
        .await
        .expect("user may curate");
    let meta = queries::get_concept_meta(pool, entity_id)
        .await
        .expect("get meta")
        .expect("meta exists");
    assert_eq!(meta.status, "canonical");
}

/// Re-running the auto-classifier must not clobber a curated row.
#[tokio::test]
async fn upsert_meta_is_curation_safe() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();
    let entity_id = insert_concept(pool, "CurationConcept").await;

    queries::upsert_concept_meta(pool, entity_id, Facet::Algorithm, "topic_seed", None)
        .await
        .expect("initial classify");
    queries::set_concept_status(pool, entity_id, ConceptStatus::Canonical, Actor::User)
        .await
        .expect("curate");

    // A later auto-classify with a *different* facet must be ignored.
    queries::upsert_concept_meta(pool, entity_id, Facet::Security, "topic_seed", None)
        .await
        .expect("re-classify");

    let meta = queries::get_concept_meta(pool, entity_id)
        .await
        .expect("get meta")
        .expect("meta exists");
    assert_eq!(meta.facet, "algorithm", "curated facet must not be re-classified");
    assert_eq!(meta.status, "canonical", "curated status must be preserved");
}

/// An invariant anchored to a file surfaces via `invariants_for_file` — the
/// mettail-rust use case in miniature.
#[tokio::test]
async fn invariants_for_file_returns_anchored_invariant() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) \
         VALUES ('/t', '/t/mettail-rust', 'mettail-rust') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("insert project");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files \
            (project_id, path, relative_path, language, size_bytes, line_count, modified_at) \
         VALUES ($1, '/t/mettail-rust/src/parser.rs', 'src/parser.rs', 'rust', 100, 5, now()) \
         RETURNING id",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await
    .expect("insert file");

    let entity_id = insert_concept(pool, "AmbiguityPropagation").await;
    queries::upsert_concept_meta(pool, entity_id, Facet::Invariant, "agent", Some(project_id))
        .await
        .expect("upsert invariant meta");
    sqlx::query(
        "UPDATE ontology_concept_meta \
         SET constraint_text = 'ambiguity must propagate end-to-end until evidence rejects it' \
         WHERE entity_id = $1",
    )
    .bind(entity_id)
    .execute(pool)
    .await
    .expect("set constraint text");

    queries::memory_anchor_entity(pool, entity_id, Some(file_id), None, None, None, None, "concept_code")
        .await
        .expect("anchor concept to file");

    let invs = queries::invariants_for_file(pool, file_id)
        .await
        .expect("invariants_for_file");
    assert_eq!(invs.len(), 1, "the anchored invariant should surface");
    assert!(
        invs[0]
            .constraint_text
            .as_deref()
            .unwrap_or_default()
            .contains("ambiguity"),
        "constraint text should be returned"
    );
}
