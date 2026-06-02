//! Real-SQL oracle for Phase-10 catalog migration: software_patterns rows become
//! `design_pattern` concepts `is_a` their `paradigm` concept, canonical, with the
//! catalog kind/slug preserved as attributes — and the migration is idempotent.
//! Self-skips with no test DB.

use pgmcp::cron::ontology_migrate::run_ontology_migrate;
use pgmcp::db::queries;
use sqlx::PgPool;

async fn seed_pattern(pool: &PgPool, slug: &str, name: &str, kind: &str, category: &str) {
    sqlx::query(
        "INSERT INTO software_patterns \
            (slug, name, kind, category, summary, intent, problem, solution, consequences) \
         VALUES ($1, $2, $3, $4, 's', 'i', 'p', 'sol', 'c')",
    )
    .bind(slug)
    .bind(name)
    .bind(kind)
    .bind(category)
    .execute(pool)
    .await
    .expect("seed pattern");
}

async fn migration_concept_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM memory_entities \
         WHERE source = 'migration'::memory_source AND valid_to IS NULL",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn pattern_catalog_migrates_to_concept_hierarchy() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();
    seed_pattern(pool, "visitor", "Visitor", "pattern", "gof").await;
    seed_pattern(pool, "observer", "Observer", "pattern", "gof").await;
    seed_pattern(pool, "god-object", "God Object", "anti_pattern", "code_smells").await;

    run_ontology_migrate(pool).await.expect("migrate");

    // Visitor → a canonical design_pattern concept.
    let vid = queries::resolve_concept(pool, "Visitor")
        .await
        .unwrap()
        .expect("Visitor concept");
    let meta = queries::get_concept_meta(pool, vid).await.unwrap().unwrap();
    assert_eq!(meta.facet, "design_pattern");
    assert_eq!(meta.status, "canonical");

    // gof → a paradigm concept; Visitor is_a gof.
    let gid = queries::resolve_concept(pool, "gof")
        .await
        .unwrap()
        .expect("gof paradigm concept");
    assert_eq!(
        queries::get_concept_meta(pool, gid).await.unwrap().unwrap().facet,
        "paradigm"
    );
    let isa: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT from_entity_id, to_entity_id FROM memory_relations \
         WHERE relation_type = 'is_a' AND valid_to IS NULL",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert!(isa.contains(&(vid, gid)), "Visitor is_a gof");

    // Catalog kind preserved as an attribute.
    let kind: Option<String> = sqlx::query_scalar(
        "SELECT value FROM ontology_concept_attr WHERE entity_id = $1 AND key = 'pattern_kind'",
    )
    .bind(vid)
    .fetch_optional(pool)
    .await
    .unwrap();
    assert_eq!(kind.as_deref(), Some("pattern"));

    // Idempotent: a second migration adds no new migration concepts.
    let before = migration_concept_count(pool).await;
    run_ontology_migrate(pool).await.expect("re-migrate");
    let after = migration_concept_count(pool).await;
    assert_eq!(before, after, "migration must be idempotent");
}
