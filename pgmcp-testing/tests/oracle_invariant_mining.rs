//! Real-SQL oracle for Phase-3 invariant mining. Seeds the *same* invariant
//! sentence in an ADR, a mandate file, and a commit; runs the deterministic
//! miner; asserts the three sources collapse onto ONE `invariant` concept that
//! carries three evidence rows (kinds adr/mandate/commit). Self-skips with no DB.

use pgmcp::config::OntologyConfig;
use pgmcp::cron::ontology_invariants::run_ontology_invariants;
use pgmcp::ontology::mine::normalize_invariant_name;
use sqlx::PgPool;

const RULE: &str = "ambiguity must propagate end-to-end until evidence rejects it";

async fn seed_file(pool: &PgPool, project_id: i32, rel: &str, content: &str) {
    sqlx::query(
        "INSERT INTO indexed_files \
            (project_id, path, relative_path, language, size_bytes, line_count, modified_at, content) \
         VALUES ($1, $2, $3, 'markdown', $4, 5, now(), $5)",
    )
    .bind(project_id)
    .bind(format!("/t/mettail-rust/{rel}"))
    .bind(rel)
    .bind(content.len() as i64)
    .bind(content)
    .execute(pool)
    .await
    .expect("seed indexed file");
}

#[tokio::test]
async fn mining_merges_three_sources_onto_one_invariant() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) \
         VALUES ('/t', '/t/mettail-rust', 'mettail-rust') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("seed project");

    // (1) ADR with the rule as its sole cue line.
    seed_file(
        pool,
        project_id,
        "docs/decisions/099-ambiguity-propagation.md",
        &format!("# ADR-099: Ambiguity propagation\n\n## Decision\n{RULE}\n"),
    )
    .await;
    // (2) Mandate file with the same rule as a bullet.
    seed_file(
        pool,
        project_id,
        "CLAUDE.md",
        &format!("# Project rules\n\n- {RULE}\n"),
    )
    .await;
    // (3) Commit whose subject is the rule.
    sqlx::query(
        "INSERT INTO git_commits (project_id, commit_hash, author, author_date, subject) \
         VALUES ($1, 'deadbeef', 'Dev', now(), $2)",
    )
    .bind(project_id)
    .bind(RULE)
    .execute(pool)
    .await
    .expect("seed commit");

    // Run the deterministic miner.
    let cfg = OntologyConfig::default();
    run_ontology_invariants(pool, &cfg)
        .await
        .expect("invariant mining run");

    // One concept, by the normalized merge name.
    let name = normalize_invariant_name(RULE);
    let entity_id: i64 = sqlx::query_scalar(
        "SELECT id FROM memory_entities \
         WHERE name = $1 AND entity_type = 'concept' AND valid_to IS NULL",
    )
    .bind(&name)
    .fetch_one(pool)
    .await
    .unwrap_or_else(|e| panic!("expected one merged concept named `{name}`: {e}"));

    // It is an invariant carrying the constraint text.
    let (facet, constraint): (String, Option<String>) = sqlx::query_as(
        "SELECT facet, constraint_text FROM ontology_concept_meta WHERE entity_id = $1",
    )
    .bind(entity_id)
    .fetch_one(pool)
    .await
    .expect("meta row");
    assert_eq!(facet, "invariant");
    assert!(
        constraint
            .as_deref()
            .unwrap_or_default()
            .contains("ambiguity"),
        "constraint text should be the mined rule"
    );

    // Three evidence rows, one per source kind.
    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT evidence_kind FROM ontology_concept_evidence \
         WHERE entity_id = $1 ORDER BY evidence_kind",
    )
    .bind(entity_id)
    .fetch_all(pool)
    .await
    .expect("evidence kinds");
    assert_eq!(
        kinds,
        vec![
            "adr".to_string(),
            "commit".to_string(),
            "mandate".to_string()
        ],
        "expected adr+commit+mandate evidence, got {kinds:?}"
    );

    // Idempotent: a second run adds no new evidence.
    let before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM ontology_concept_evidence WHERE entity_id = $1")
            .bind(entity_id)
            .fetch_one(pool)
            .await
            .unwrap();
    run_ontology_invariants(pool, &cfg)
        .await
        .expect("second run");
    let after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM ontology_concept_evidence WHERE entity_id = $1")
            .bind(entity_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(
        before, after,
        "mining must be idempotent (provenance-keyed)"
    );
}
