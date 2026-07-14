//! Real-SQL oracle for the Phase-6 ontology tool query layer: agent-asserted
//! invariants are candidate-only (trust boundary), concept creation + facet-
//! filtered search, and hierarchy-edge/ resolve helpers. The MCP handler/body
//! layer is a thin compile-checked wrapper over these. Self-skips with no DB.

use pgmcp::db::queries;
use pgmcp::ontology::edge::OntologyRelation;
use pgmcp::ontology::facet::{ConceptStatus, Facet};
use pgmcp::tracker::transition::Actor;
use serde_json::json;
use uuid::Uuid;

use crate::common::{server_with_pool, text_of};

async fn seed_project_and_file(
    pool: &sqlx::PgPool,
    name: &str,
    root: &str,
    relative_path: &str,
) -> (i32, i64) {
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name)
         VALUES ($1, $2, $3)
         RETURNING id",
    )
    .bind(root)
    .bind(root)
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("project");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files
            (project_id, path, relative_path, language, size_bytes, line_count, modified_at)
         VALUES ($1, $2, $3, 'rust', 100, 5, now())
         RETURNING id",
    )
    .bind(project_id)
    .bind(format!("{root}/{relative_path}"))
    .bind(relative_path)
    .fetch_one(pool)
    .await
    .expect("file");
    (project_id, file_id)
}

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
    let ev = queries::list_concept_evidence(pool, eid)
        .await
        .expect("evidence");
    assert!(
        ev.iter().any(|e| e.evidence_kind == "agent"),
        "agent-kind evidence recorded"
    );
}

#[tokio::test]
async fn invariants_for_file_tool_trims_and_returns_anchor() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();
    let suffix = Uuid::now_v7().simple().to_string();
    let relative_path = format!("src/parser-{suffix}.rs");
    let (_project_id, file_id) = seed_project_and_file(
        pool,
        &format!("ontology-inv-{suffix}"),
        &format!("/ws/ontology-inv-{suffix}"),
        &relative_path,
    )
    .await;
    let entity_id = queries::agent_assert_invariant(
        pool,
        &format!("ParserInvariant{suffix}"),
        "parser input must be validated before parse",
        "tool regression",
        Some(file_id),
    )
    .await
    .expect("assert invariant");

    let server = server_with_pool(pool.clone());
    let result = server
        .call_tool_cli(
            "ontology_invariants_for_file",
            json!({"file": format!("  {relative_path}  ")}),
        )
        .await
        .expect("invariants tool");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["file"].as_str(), Some(relative_path.as_str()));
    assert_eq!(v["file_id"].as_i64(), Some(file_id));
    let invariants = v["invariants"].as_array().expect("invariants");
    assert!(
        invariants
            .iter()
            .any(|row| row["entity_id"].as_i64() == Some(entity_id)),
        "anchored invariant should surface: {v}"
    );
}

#[tokio::test]
async fn invariants_for_file_rejects_ambiguous_relative_path() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();
    let suffix = Uuid::now_v7().simple().to_string();
    let relative_path = format!("src/lib-{suffix}.rs");
    for label in ["a", "b"] {
        seed_project_and_file(
            pool,
            &format!("ontology-amb-{label}-{suffix}"),
            &format!("/ws/ontology-amb-{label}-{suffix}"),
            &relative_path,
        )
        .await;
    }

    let server = server_with_pool(pool.clone());
    let err = server
        .call_tool_cli(
            "ontology_invariants_for_file",
            json!({"file": relative_path}),
        )
        .await
        .expect_err("ambiguous relative path must fail closed");
    assert!(
        err.to_string().contains("ambiguous across indexed files"),
        "unexpected ambiguity error: {err}"
    );
}

#[tokio::test]
async fn invariants_for_file_treats_wildcards_literally() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();
    let suffix = Uuid::now_v7().simple().to_string();
    let relative_path = format!("src/lib-{suffix}.rs");
    seed_project_and_file(
        pool,
        &format!("ontology-wild-{suffix}"),
        &format!("/ws/ontology-wild-{suffix}"),
        &relative_path,
    )
    .await;

    let server = server_with_pool(pool.clone());
    let result = server
        .call_tool_cli("ontology_invariants_for_file", json!({"file": "%"}))
        .await
        .expect("wildcard should be literal and miss");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["file"].as_str(), Some("%"));
    assert_eq!(v["note"].as_str(), Some("file not indexed"));
    assert!(v["invariants"].as_array().expect("invariants").is_empty());
}

#[tokio::test]
async fn create_concept_and_facet_filtered_search() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();

    let (cid, created) = queries::create_concept(
        pool,
        "Formal Verification Systems",
        Facet::Collection,
        Actor::Agent,
    )
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
    let in_algorithm = queries::search_concepts_by_name(pool, "Formal", Some(Facet::Algorithm), 10)
        .await
        .expect("search wrong facet");
    assert!(!in_algorithm.iter().any(|h| h.entity_id == cid));
}

#[tokio::test]
async fn create_concept_rejects_blank_and_oversized_names() {
    let db = pgmcp_testing::require_test_db!();
    let server = server_with_pool(db.pool().clone());

    assert!(
        server
            .call_tool_cli(
                "ontology_create_concept",
                json!({"name": "   ", "facet": "tool"})
            )
            .await
            .is_err(),
        "blank concept names must fail before any DB write"
    );

    let oversized = "x".repeat(queries::ONTOLOGY_CONCEPT_MAX_NAME_CHARS + 1);
    assert!(
        server
            .call_tool_cli(
                "ontology_create_concept",
                json!({"name": oversized, "facet": "tool"})
            )
            .await
            .is_err(),
        "oversized concept names must fail before any DB write"
    );

    assert!(
        server
            .call_tool_cli(
                "ontology_create_concept",
                json!({"name": "Z3 Solver", "facet": "   "})
            )
            .await
            .is_err(),
        "blank facets must fail before any DB write"
    );

    let result = server
        .call_tool_cli(
            "ontology_create_concept",
            json!({"name": "  Z3 Solver  ", "facet": " tool "}),
        )
        .await
        .expect("trimmed facet accepted");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["name"].as_str(), Some("Z3 Solver"));
    assert_eq!(v["facet"].as_str(), Some("tool"));
    assert_eq!(v["status"].as_str(), Some("candidate"));
}

#[tokio::test]
async fn create_concept_tool_returns_actual_curated_metadata() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();

    let (cid, _) = queries::create_concept(pool, "Curated Parser", Facet::Component, Actor::User)
        .await
        .expect("seed concept");
    queries::set_concept_status(pool, cid, ConceptStatus::Canonical, Actor::User)
        .await
        .expect("curate");

    let server = server_with_pool(pool.clone());
    let result = server
        .call_tool_cli(
            "ontology_create_concept",
            json!({"name": "  Curated Parser  ", "facet": "collection"}),
        )
        .await
        .expect("tool call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["entity_id"].as_i64(), Some(cid));
    assert_eq!(v["created"].as_bool(), Some(false));
    assert_eq!(v["name"].as_str(), Some("Curated Parser"));
    assert_eq!(v["facet"].as_str(), Some("component"));
    assert_eq!(v["status"].as_str(), Some("canonical"));
}

#[tokio::test]
async fn concurrent_create_concept_is_single_active_entity() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool().clone();
    let name = "Concurrent Formal Verification Tool";

    let mut joins = Vec::new();
    for _ in 0..16 {
        let pool = pool.clone();
        joins.push(tokio::spawn(async move {
            queries::create_concept(&pool, name, Facet::Tool, Actor::Agent).await
        }));
    }

    let mut ids = Vec::new();
    let mut created = 0;
    for join in joins {
        let (id, was_created) = join.await.expect("task join").expect("create concept");
        ids.push(id);
        created += i32::from(was_created);
    }

    assert_eq!(created, 1, "exactly one concurrent writer inserts");
    assert!(
        ids.iter().all(|id| *id == ids[0]),
        "all concurrent writers resolve the same active entity id: {ids:?}"
    );

    let active_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memory_entities
         WHERE name = $1 AND entity_type = 'concept' AND valid_to IS NULL",
    )
    .bind(name)
    .fetch_one(&pool)
    .await
    .expect("count active concepts");
    assert_eq!(active_count, 1);
}

#[tokio::test]
async fn hierarchy_edges_and_resolve() {
    let db = pgmcp_testing::require_test_db!();
    let pool = db.pool();

    let (parent, _) = queries::create_concept(pool, "Parser", Facet::Component, Actor::User)
        .await
        .expect("parent");
    let (child, _) = queries::create_concept(
        pool,
        "RecursiveDescentParser",
        Facet::Component,
        Actor::User,
    )
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

    assert_eq!(
        queries::resolve_concept(pool, "Parser").await.unwrap(),
        Some(parent)
    );
    assert_eq!(
        queries::resolve_concept(pool, &parent.to_string())
            .await
            .unwrap(),
        Some(parent)
    );
    assert_eq!(
        queries::resolve_concept(pool, "NoSuchConcept")
            .await
            .unwrap(),
        None
    );
}
