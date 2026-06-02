//! Real-SQL oracle for Phase-7 proactive surfacing: the read-only digest
//! `collect_ontology` collector surfaces a file's canonical invariants at High
//! severity and labels non-canonical (agent-asserted) ones `(candidate)` so
//! unverified assertions are visibly provisional. (The read-only trust boundary
//! itself is enforced by `digest_trust_boundary.rs`.) Self-skips with no DB.

use pgmcp::config::DigestConfig;
use pgmcp::db::queries;
use pgmcp::digest::{DigestCategory, DigestSeverity, compose_digest};
use pgmcp::ontology::facet::ConceptStatus;
use pgmcp::tracker::transition::Actor;
use sqlx::PgPool;

async fn seed_project_file(pool: &PgPool) -> (i32, i64) {
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) \
         VALUES ('/t', '/t/mettail-rust', 'mettail-rust') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("project");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files \
            (project_id, path, relative_path, language, size_bytes, line_count, modified_at) \
         VALUES ($1, '/t/mettail-rust/src/parser.rs', 'src/parser.rs', 'rust', 100, 5, now()) \
         RETURNING id",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await
    .expect("file");
    (project_id, file_id)
}

#[tokio::test]
async fn ontology_invariants_surface_in_digest() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();
    let (project_id, file_id) = seed_project_file(pool).await;

    // A canonical invariant (curator-promoted) and a candidate (agent-asserted),
    // both anchored to the project's file.
    let canonical = queries::agent_assert_invariant(
        pool,
        "InputValidation",
        "must always validate the input token stream",
        "ADR",
        Some(file_id),
    )
    .await
    .expect("assert canonical");
    queries::set_concept_status(pool, canonical, ConceptStatus::Canonical, Actor::User)
        .await
        .expect("curate");
    queries::agent_assert_invariant(
        pool,
        "AmbiguityPropagation",
        "never disambiguate prematurely over the parse tree",
        "agent",
        Some(file_id),
    )
    .await
    .expect("assert candidate");

    let cfg = DigestConfig::default(); // include_ontology defaults true
    let digest = compose_digest(pool, Some(project_id), None, &cfg).await;
    let onto: Vec<_> = digest
        .items
        .iter()
        .filter(|i| i.category == DigestCategory::Ontology)
        .collect();

    assert!(
        onto.iter().any(|i| i.severity == DigestSeverity::High
            && i.text
                .contains("must always validate the input token stream")),
        "canonical invariant surfaces at High severity"
    );
    assert!(
        onto.iter()
            .any(|i| i.text.contains("(candidate)")
                && i.text.contains("never disambiguate prematurely")),
        "candidate invariant surfaces but is labeled provisional"
    );
}
