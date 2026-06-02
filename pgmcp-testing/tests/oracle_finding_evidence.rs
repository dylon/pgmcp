//! Real-SQL oracle for Phase-11 producer integration: a concurrency finding (v22)
//! touching a file an ontology concept is anchored to becomes `finding` evidence
//! on that concept, idempotently. Self-skips with no test DB.

use pgmcp::cron::ontology_integrate::run_ontology_integrate;
use pgmcp::db::queries;
use pgmcp::ontology::facet::Facet;
use pgmcp::tracker::transition::Actor;
use sqlx::PgPool;

async fn seed_project_file(pool: &PgPool) -> (i32, i64) {
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) \
         VALUES ('/t', '/t/proj', 'proj') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("project");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files \
            (project_id, path, relative_path, language, size_bytes, line_count, modified_at) \
         VALUES ($1, '/t/proj/src/lock.rs', 'src/lock.rs', 'rust', 100, 5, now()) RETURNING id",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await
    .expect("file");
    (project_id, file_id)
}

#[tokio::test]
async fn concurrency_findings_attach_as_concept_evidence() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();
    let (project_id, file_id) = seed_project_file(pool).await;

    // A concept governing the file.
    let (cid, _) = queries::create_concept(pool, "ParserLocking", Facet::Concurrency, Actor::User)
        .await
        .unwrap();
    queries::memory_anchor_entity(
        pool,
        cid,
        Some(file_id),
        None,
        None,
        None,
        None,
        "concept_code",
    )
    .await
    .unwrap();

    // A concurrency finding on that file.
    sqlx::query(
        "INSERT INTO concurrency_findings \
            (project_id, finding_kind, severity, provenance_key, file_id, title) \
         VALUES ($1, 'lock_contention', 'high', 'cf-test-1', $2, 'lock contention in parser')",
    )
    .bind(project_id)
    .bind(file_id)
    .execute(pool)
    .await
    .expect("seed finding");

    run_ontology_integrate(pool).await.expect("integrate");

    let evidence = queries::list_concept_evidence(pool, cid).await.unwrap();
    assert!(
        evidence.iter().any(|e| e.evidence_kind == "finding"
            && e.detail.as_deref() == Some("lock contention in parser")),
        "the finding should attach as concept evidence, got {evidence:?}"
    );

    // Idempotent.
    let before = queries::count_concept_evidence(pool, cid).await.unwrap();
    run_ontology_integrate(pool).await.expect("re-integrate");
    let after = queries::count_concept_evidence(pool, cid).await.unwrap();
    assert_eq!(before, after, "finding evidence attach must be idempotent");
}
