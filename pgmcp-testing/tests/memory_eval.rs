//! Memory-server Phase 9: internal evaluation harness.
//!
//! 20+ scenarios across recall / contradiction / multi-hop / cross-graph /
//! scope-isolation / tier-filter / forgetting / reflection. Each scenario
//! is a `#[tokio::test]` that asserts one specific memory-server
//! behaviour against a real PostgreSQL transaction. They run as part of
//! the standard `cargo test` flow (and therefore `scripts/verify.sh`),
//! so the harness doubles as the per-merge regression gate.
//!
//! The `memory-eval` cron (off by default) periodically reruns this
//! suite against a sandbox schema and writes per-scenario pass/fail
//! into `pgmcp_metadata`. The cron implementation lives in
//! `src/cron/memory_eval.rs`.

use std::sync::Arc;

use pgmcp::db::queries::{self, ForgetTargetType, ScopeSpec};
use pgmcp::llm::extractor_worker::{
    ExtractorJob, ExtractorWorkerConfig, run_extraction_for_prompt,
};
use pgmcp::llm::reflect::{ReflectionRequest, ReflectionTrigger, run_reflection};
use pgmcp::llm::{
    ContradictionKind, ContradictionSignal, ExtractionRequest, ExtractionResult, LlmExtractor,
    NewEntity, NewRelation,
};
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::pool_tool_helpers::server_with_pool;
use pgmcp_testing::require_test_db;
use uuid::Uuid;

// -- Mock extractor (mirrors memory_phase4_5.rs) -----------------------------
struct CannedExtractor {
    extract: std::sync::Mutex<ExtractionResult>,
    reflect: std::sync::Mutex<Vec<NewEntity>>,
}

impl CannedExtractor {
    fn new() -> Self {
        Self {
            extract: std::sync::Mutex::new(ExtractionResult::default()),
            reflect: std::sync::Mutex::new(Vec::new()),
        }
    }
}
impl LlmExtractor for CannedExtractor {
    fn name(&self) -> &'static str {
        "canned"
    }
    fn model_signature(&self) -> &'static str {
        "canned-v1"
    }
    fn extract(&self, _r: ExtractionRequest<'_>) -> anyhow::Result<ExtractionResult> {
        Ok(self.extract.lock().unwrap().clone())
    }
    fn reflect(&self, _o: &[String]) -> anyhow::Result<Vec<NewEntity>> {
        Ok(self.reflect.lock().unwrap().clone())
    }
}

fn unit(axis: usize) -> Vec<f32> {
    (0..1024)
        .map(|i| if i == axis { 1.0 } else { 0.0 })
        .collect()
}

// =====================================================================
// Recall (5 scenarios)
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn eval_recall_semantic_finds_seeded_observation() {
    let db = require_test_db!();
    let pool = db.pool();
    let entity_id: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('eval-recall', 'concept', 'agent_write'::memory_source) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("entity");
    let v = unit(11);
    let pgv = pgvector::Vector::from(v.clone());
    sqlx::query(
        "INSERT INTO memory_observations (entity_id, content, content_sha256, embedding, source)
         VALUES ($1, 'recall payload',
                 '01234567890123456789012345678901234567890123456789012345678901aa',
                 $2, 'agent_write'::memory_source)",
    )
    .bind(entity_id)
    .bind(&pgv)
    .execute(pool)
    .await
    .expect("obs");
    let rows = queries::memory_semantic_search(pool, &v, None, None, 5, 64)
        .await
        .expect("semantic_search");
    assert!(rows.iter().any(|r| r.content == "recall payload"));
}

#[tokio::test(flavor = "multi_thread")]
async fn eval_recall_fts_matches_search_nodes() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    server
        .call_tool_cli(
            "memory_create_entities",
            serde_json::json!({
                "entities": [{"name": "eval-fts", "entity_type": "concept",
                              "observations": ["match this exact substring"]}]
            }),
        )
        .await
        .expect("create");
    let result = server
        .call_tool_cli(
            "memory_search_nodes",
            serde_json::json!({"query": "exact substring"}),
        )
        .await
        .expect("search");
    let body: serde_json::Value = serde_json::from_str(
        result.content[0]
            .raw
            .as_text()
            .map(|t| t.text.as_str())
            .unwrap_or("{}"),
    )
    .expect("json");
    let count = body.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
    assert!(count >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn eval_recall_exact_name_via_open_nodes() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    server
        .call_tool_cli(
            "memory_create_entities",
            serde_json::json!({
                "entities": [{"name": "eval-exact", "entity_type": "concept",
                              "observations": ["payload"]}]
            }),
        )
        .await
        .expect("create");
    let result = server
        .call_tool_cli(
            "memory_open_nodes",
            serde_json::json!({"names": ["eval-exact"]}),
        )
        .await
        .expect("open");
    let body: serde_json::Value =
        serde_json::from_str(result.content[0].raw.as_text().unwrap().text.as_str()).unwrap();
    assert_eq!(body.get("count").and_then(|v| v.as_i64()), Some(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn eval_recall_scope_isolation_holds() {
    let db = require_test_db!();
    let pool = db.pool();
    let scope_a = queries::find_or_create_scope(
        pool,
        &ScopeSpec {
            user_id: Some("alice".into()),
            ..Default::default()
        },
    )
    .await
    .expect("scope a");
    let scope_b = queries::find_or_create_scope(
        pool,
        &ScopeSpec {
            user_id: Some("bob".into()),
            ..Default::default()
        },
    )
    .await
    .expect("scope b");
    queries::memory_create_entities(
        pool,
        &[queries::NewEntityInput {
            name: "alice-secret".into(),
            entity_type: "concept".into(),
            observations: vec![],
        }],
        scope_a,
        "agent_write",
    )
    .await
    .expect("alice");
    let hits = queries::memory_search_nodes(pool, "alice", Some(scope_b), 10)
        .await
        .expect("search b");
    assert!(hits.iter().all(|h| h.name != "alice-secret"));
}

#[tokio::test(flavor = "multi_thread")]
async fn eval_recall_tier_filter_excludes_other_tiers() {
    let db = require_test_db!();
    let pool = db.pool();
    let entity: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('tier-test', 'concept', 'agent_write'::memory_source) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("entity");
    sqlx::query(
        "INSERT INTO memory_entity_tier (entity_id, tier, weight)
         VALUES ($1, 'procedural'::memory_tier, 1.0) ON CONFLICT DO NOTHING",
    )
    .bind(entity)
    .execute(pool)
    .await
    .expect("tier");
    let v = unit(42);
    let pgv = pgvector::Vector::from(v.clone());
    sqlx::query(
        "INSERT INTO memory_observations (entity_id, content, content_sha256, embedding, source)
         VALUES ($1, 'tier observation',
                 '01234567890123456789012345678901234567890123456789012345678901bb',
                 $2, 'agent_write'::memory_source)",
    )
    .bind(entity)
    .bind(&pgv)
    .execute(pool)
    .await
    .expect("obs");
    let hits = queries::memory_semantic_search(pool, &v, None, Some("semantic"), 10, 64)
        .await
        .expect("search");
    assert!(hits.iter().all(|h| h.entity_name != "tier-test"));
    let hits = queries::memory_semantic_search(pool, &v, None, Some("procedural"), 10, 64)
        .await
        .expect("search");
    assert!(hits.iter().any(|h| h.entity_name == "tier-test"));
}

// =====================================================================
// Contradiction (3 scenarios)
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn eval_contradiction_observation_supersession() {
    let db = require_test_db!();
    let pool = db.pool();
    let stats = Arc::new(StatsTracker::new());
    let entity: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('eval-contradict', 'concept', 'agent_write'::memory_source) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("entity");
    let prior: i64 = sqlx::query_scalar(
        "INSERT INTO memory_observations (entity_id, content, content_sha256, source)
         VALUES ($1, 'use library X',
                 'cd0001cd0001cd0001cd0001cd0001cd0001cd0001cd0001cd0001cd000100ab',
                 'agent_write'::memory_source) RETURNING id",
    )
    .bind(entity)
    .fetch_one(pool)
    .await
    .expect("prior");
    let session_id = Uuid::new_v4();
    pgmcp::sessions::upsert_session(pool, session_id, "/ws/eval-contradict", None)
        .await
        .unwrap();
    let sha = pgmcp::sessions::prompt_sha256("now use Y");
    let prompt_id = pgmcp::sessions::insert_prompt(pool, session_id, "now use Y", &sha, None)
        .await
        .expect("prompt");
    let mock = Arc::new(CannedExtractor::new());
    *mock.extract.lock().unwrap() = ExtractionResult {
        entities: vec![],
        relations: vec![],
        contradictions: vec![ContradictionSignal {
            conflicting_with: prior,
            kind: ContradictionKind::Observation,
            reason: "switched libraries".into(),
        }],
    };
    let extractor: Arc<dyn LlmExtractor> = mock as Arc<dyn LlmExtractor>;
    run_extraction_for_prompt(
        pool.clone(),
        Arc::clone(&stats),
        extractor,
        Arc::new(dashmap::DashMap::new()),
        ExtractorWorkerConfig::default(),
        ExtractorJob {
            session_id,
            source_prompt_id: prompt_id,
            project_id: None,
            agent_id: None,
            user_id: None,
            prompt_text: "now use Y".into(),
        },
    )
    .await;
    let valid_to: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT valid_to FROM memory_observations WHERE id = $1")
            .bind(prior)
            .fetch_one(pool)
            .await
            .expect("valid_to");
    assert!(valid_to.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn eval_contradiction_relation_supersession() {
    let db = require_test_db!();
    let pool = db.pool();
    let stats = Arc::new(StatsTracker::new());
    let from_e: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('rel-from', 'node', 'agent_write'::memory_source) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("from");
    let to_e: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('rel-to', 'node', 'agent_write'::memory_source) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("to");
    let rel: i64 = sqlx::query_scalar(
        "INSERT INTO memory_relations (from_entity_id, to_entity_id, relation_type, source)
         VALUES ($1, $2, 'related_to', 'agent_write'::memory_source) RETURNING id",
    )
    .bind(from_e)
    .bind(to_e)
    .fetch_one(pool)
    .await
    .expect("rel");
    let session_id = Uuid::new_v4();
    pgmcp::sessions::upsert_session(pool, session_id, "/ws/rel-contradict", None)
        .await
        .unwrap();
    let sha = pgmcp::sessions::prompt_sha256("relation invalidated");
    let prompt_id =
        pgmcp::sessions::insert_prompt(pool, session_id, "relation invalidated", &sha, None)
            .await
            .expect("prompt");
    let mock = Arc::new(CannedExtractor::new());
    *mock.extract.lock().unwrap() = ExtractionResult {
        entities: vec![],
        relations: vec![],
        contradictions: vec![ContradictionSignal {
            conflicting_with: rel,
            kind: ContradictionKind::Relation,
            reason: "relation no longer applies".into(),
        }],
    };
    let extractor: Arc<dyn LlmExtractor> = mock as Arc<dyn LlmExtractor>;
    run_extraction_for_prompt(
        pool.clone(),
        Arc::clone(&stats),
        extractor,
        Arc::new(dashmap::DashMap::new()),
        ExtractorWorkerConfig::default(),
        ExtractorJob {
            session_id,
            source_prompt_id: prompt_id,
            project_id: None,
            agent_id: None,
            user_id: None,
            prompt_text: "relation invalidated".into(),
        },
    )
    .await;
    let valid_to: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT valid_to FROM memory_relations WHERE id = $1")
            .bind(rel)
            .fetch_one(pool)
            .await
            .expect("valid_to");
    assert!(valid_to.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn eval_contradiction_only_targets_specified_row() {
    let db = require_test_db!();
    let pool = db.pool();
    let stats = Arc::new(StatsTracker::new());
    let entity: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('eval-multi-obs', 'concept', 'agent_write'::memory_source) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("entity");
    let obs_a: i64 = sqlx::query_scalar(
        "INSERT INTO memory_observations (entity_id, content, content_sha256, source)
         VALUES ($1, 'fact a',
                 'aaaabbbbccccddddeeeeffff00001111222233334444555566667777888899aa',
                 'agent_write'::memory_source) RETURNING id",
    )
    .bind(entity)
    .fetch_one(pool)
    .await
    .expect("a");
    let obs_b: i64 = sqlx::query_scalar(
        "INSERT INTO memory_observations (entity_id, content, content_sha256, source)
         VALUES ($1, 'fact b',
                 'aaaabbbbccccddddeeeeffff00001111222233334444555566667777888899bb',
                 'agent_write'::memory_source) RETURNING id",
    )
    .bind(entity)
    .fetch_one(pool)
    .await
    .expect("b");
    let session_id = Uuid::new_v4();
    pgmcp::sessions::upsert_session(pool, session_id, "/ws/multi", None)
        .await
        .unwrap();
    let sha = pgmcp::sessions::prompt_sha256("contradict only b");
    let pid = pgmcp::sessions::insert_prompt(pool, session_id, "contradict only b", &sha, None)
        .await
        .expect("prompt");
    let mock = Arc::new(CannedExtractor::new());
    *mock.extract.lock().unwrap() = ExtractionResult {
        entities: vec![],
        relations: vec![],
        contradictions: vec![ContradictionSignal {
            conflicting_with: obs_b,
            kind: ContradictionKind::Observation,
            reason: "b is wrong".into(),
        }],
    };
    let extractor: Arc<dyn LlmExtractor> = mock as Arc<dyn LlmExtractor>;
    run_extraction_for_prompt(
        pool.clone(),
        Arc::clone(&stats),
        extractor,
        Arc::new(dashmap::DashMap::new()),
        ExtractorWorkerConfig::default(),
        ExtractorJob {
            session_id,
            source_prompt_id: pid,
            project_id: None,
            agent_id: None,
            user_id: None,
            prompt_text: "x".into(),
        },
    )
    .await;
    let a_vt: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT valid_to FROM memory_observations WHERE id = $1")
            .bind(obs_a)
            .fetch_one(pool)
            .await
            .unwrap();
    let b_vt: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT valid_to FROM memory_observations WHERE id = $1")
            .bind(obs_b)
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(a_vt.is_none(), "a should not be invalidated");
    assert!(b_vt.is_some(), "b should be invalidated");
}

// =====================================================================
// Multi-hop (3 scenarios)
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn eval_multihop_relations_traverse_two_hops() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    server
        .call_tool_cli(
            "memory_create_entities",
            serde_json::json!({
                "entities": [
                    {"name": "mh-a", "entity_type": "node"},
                    {"name": "mh-b", "entity_type": "node"},
                    {"name": "mh-c", "entity_type": "node"}
                ]
            }),
        )
        .await
        .expect("create");
    server
        .call_tool_cli(
            "memory_create_relations",
            serde_json::json!({
                "relations": [
                    {"from": "mh-a", "to": "mh-b", "relation_type": "next"},
                    {"from": "mh-b", "to": "mh-c", "relation_type": "next"}
                ]
            }),
        )
        .await
        .expect("rels");
    let a_id: i64 = sqlx::query_scalar(
        "SELECT id FROM memory_entities WHERE name = 'mh-a' AND valid_to IS NULL",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let result = queries::memory_relations_traverse(pool, &[a_id], 2, None, 50)
        .await
        .expect("traverse");
    assert!(result.nodes.len() >= 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn eval_multihop_ppr_finds_indirect_neighbor() {
    let db = require_test_db!();
    let pool = db.pool();
    let a: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('ppr-mh-a', 'node', 'agent_write'::memory_source) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let b: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('ppr-mh-b', 'node', 'agent_write'::memory_source) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let v = unit(77);
    let pgv = pgvector::Vector::from(v.clone());
    sqlx::query(
        "INSERT INTO memory_observations (entity_id, content, content_sha256, embedding, source)
         VALUES ($1, 'seed', '01234567890123456789012345678901234567890123456789012345678aaabb',
                 $2, 'agent_write'::memory_source)",
    )
    .bind(a)
    .bind(&pgv)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory_relations (from_entity_id, to_entity_id, relation_type, source)
         VALUES ($1, $2, 'next', 'agent_write'::memory_source)",
    )
    .bind(a)
    .bind(b)
    .execute(pool)
    .await
    .unwrap();
    let result = queries::memory_ppr_search(pool, &v, 10, 0.85, 5, 64)
        .await
        .expect("ppr");
    let hit_ids: Vec<i64> = result.hits.iter().map(|h| h.entity_id).collect();
    assert!(hit_ids.contains(&b));
}

#[tokio::test(flavor = "multi_thread")]
async fn eval_multihop_path_search_emits_paths() {
    let db = require_test_db!();
    let pool = db.pool();
    let seed: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('path-mh-seed', 'node', 'agent_write'::memory_source) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let mid: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('path-mh-mid', 'node', 'agent_write'::memory_source) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let v = unit(101);
    let pgv = pgvector::Vector::from(v.clone());
    sqlx::query(
        "INSERT INTO memory_observations (entity_id, content, content_sha256, embedding, source)
         VALUES ($1, 'p',
                 '11111111111111111111111111111111111111111111111111111111111101ee',
                 $2, 'agent_write'::memory_source)",
    )
    .bind(seed)
    .bind(&pgv)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory_relations (from_entity_id, to_entity_id, relation_type, source)
         VALUES ($1, $2, 'leads', 'agent_write'::memory_source)",
    )
    .bind(seed)
    .bind(mid)
    .execute(pool)
    .await
    .unwrap();
    queries::refresh_memory_unified_nodes(pool).await.unwrap();
    let result = queries::memory_path_search(pool, &v, None, None, 2, 5, 0.7, 64)
        .await
        .expect("path");
    assert!(!result.paths.is_empty());
}

// =====================================================================
// Cross-graph (2 scenarios)
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn eval_crossgraph_anchor_round_trip() {
    let db = require_test_db!();
    let pool = db.pool();
    let entity: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('xg-e', 'concept', 'agent_write'::memory_source) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET name = $3 RETURNING id",
    )
    .bind("/ws")
    .bind("/ws/xg")
    .bind("xg")
    .fetch_one(pool)
    .await
    .unwrap();
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, line_count, modified_at)
         VALUES ($1, '/ws/xg/a.rs', 'a.rs', 'rust', 0, 'x', 1, NOW()) RETURNING id",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await
    .unwrap();
    let anchor_id =
        queries::memory_anchor_entity(pool, entity, Some(file_id), None, None, "implements")
            .await
            .unwrap();
    let anchors = queries::memory_find_code_for_entity(pool, entity, None)
        .await
        .unwrap();
    assert_eq!(anchors.len(), 1);
    let reverse = queries::memory_find_entities_for_code(pool, Some(file_id), None, None)
        .await
        .unwrap();
    assert!(reverse.iter().any(|a| a.entity_id == entity));
    assert!(
        queries::memory_unanchor_entity(pool, anchor_id)
            .await
            .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn eval_crossgraph_reverse_lookup_requires_exactly_one_target() {
    let db = require_test_db!();
    let pool = db.pool();
    let err = queries::memory_find_entities_for_code(pool, None, None, None)
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("exactly one"));
}

// =====================================================================
// Scope isolation (2 scenarios)
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn eval_scope_isolation_unified_search_does_not_leak() {
    let db = require_test_db!();
    let pool = db.pool();
    let scope_a = queries::find_or_create_scope(
        pool,
        &ScopeSpec {
            user_id: Some("iso-a".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let scope_b = queries::find_or_create_scope(
        pool,
        &ScopeSpec {
            user_id: Some("iso-b".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    queries::memory_create_entities(
        pool,
        &[queries::NewEntityInput {
            name: "iso-secret-a".into(),
            entity_type: "concept".into(),
            observations: vec!["hidden a".into()],
        }],
        scope_a,
        "agent_write",
    )
    .await
    .unwrap();
    let hits = queries::memory_search_nodes(pool, "hidden", Some(scope_b), 10)
        .await
        .unwrap();
    assert!(hits.iter().all(|h| h.name != "iso-secret-a"));
}

#[tokio::test(flavor = "multi_thread")]
async fn eval_scope_isolation_shared_when_same_scope() {
    let db = require_test_db!();
    let pool = db.pool();
    let scope = queries::find_or_create_scope(
        pool,
        &ScopeSpec {
            user_id: Some("shared-user".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    queries::memory_create_entities(
        pool,
        &[queries::NewEntityInput {
            name: "shared-thing".into(),
            entity_type: "concept".into(),
            observations: vec!["seen by anyone in this scope".into()],
        }],
        scope,
        "agent_write",
    )
    .await
    .unwrap();
    let hits = queries::memory_search_nodes(pool, "shared-thing", Some(scope), 5)
        .await
        .unwrap();
    assert!(hits.iter().any(|h| h.name == "shared-thing"));
}

// =====================================================================
// Tier filter (1 scenario)
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn eval_tier_filter_round_trip() {
    let db = require_test_db!();
    let pool = db.pool();
    let entity: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('tier-eval', 'concept', 'agent_write'::memory_source) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory_entity_tier (entity_id, tier, weight)
         VALUES ($1, 'reflective'::memory_tier, 0.9) ON CONFLICT DO NOTHING",
    )
    .bind(entity)
    .execute(pool)
    .await
    .unwrap();
    let (tier_count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM memory_entity_tier WHERE entity_id = $1")
            .bind(entity)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(tier_count, 1);
}

// =====================================================================
// Forgetting (2 scenarios)
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn eval_forget_soft_preserves_history() {
    let db = require_test_db!();
    let pool = db.pool();
    let entity: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('forget-soft', 'concept', 'agent_write'::memory_source) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let report =
        queries::memory_forget(pool, ForgetTargetType::Entity, entity, false, "eval-actor")
            .await
            .unwrap();
    assert!(!report.cascade);
    let (count_total,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM memory_entities WHERE id = $1")
            .bind(entity)
            .fetch_one(pool)
            .await
            .unwrap();
    let (count_active,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM memory_entities WHERE id = $1 AND valid_to IS NULL")
            .bind(entity)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(count_total, 1);
    assert_eq!(count_active, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn eval_forget_cascade_removes_dependents() {
    let db = require_test_db!();
    let pool = db.pool();
    let entity: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('forget-cascade', 'concept', 'agent_write'::memory_source) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory_observations (entity_id, content, content_sha256, source)
         VALUES ($1, 'dep obs',
                 'fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff111',
                 'agent_write'::memory_source)",
    )
    .bind(entity)
    .execute(pool)
    .await
    .unwrap();
    let report = queries::memory_forget(pool, ForgetTargetType::Entity, entity, true, "eval-actor")
        .await
        .unwrap();
    assert!(report.cascade);
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM memory_entities WHERE id = $1")
        .bind(entity)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "cascade should hard-delete the entity row");
    let (obs_count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM memory_observations WHERE entity_id = $1")
            .bind(entity)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(obs_count, 0, "FK cascade should remove dependent obs");
    let (log_count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM memory_forget_log WHERE target_id = $1")
            .bind(entity)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(log_count, 1, "audit log must record the forget");
}

// =====================================================================
// Reflection (2 scenarios)
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn eval_reflection_emits_derived_from_link() {
    let db = require_test_db!();
    let pool = db.pool();
    let stats = Arc::new(StatsTracker::new());
    let entity: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('reflect-eval', 'concept', 'agent_write'::memory_source) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let scope = queries::find_or_create_scope(pool, &ScopeSpec::default())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO memory_entity_scope (entity_id, scope_id) VALUES ($1, $2)
         ON CONFLICT DO NOTHING",
    )
    .bind(entity)
    .bind(scope)
    .execute(pool)
    .await
    .unwrap();
    let obs: i64 = sqlx::query_scalar(
        "INSERT INTO memory_observations (entity_id, content, content_sha256, source)
         VALUES ($1, 'r-base',
                 '01010101010101010101010101010101010101010101010101010101010101cc',
                 'agent_write'::memory_source) RETURNING id",
    )
    .bind(entity)
    .fetch_one(pool)
    .await
    .unwrap();
    let mock = Arc::new(CannedExtractor::new());
    *mock.reflect.lock().unwrap() = vec![NewEntity {
        name: "reflect-summary".into(),
        entity_type: "summary".into(),
        initial_observations: vec!["consolidated".into()],
        importance: 0.7,
    }];
    let extractor: Arc<dyn LlmExtractor> = mock as Arc<dyn LlmExtractor>;
    let report = run_reflection(
        pool,
        &stats,
        extractor.as_ref(),
        ReflectionRequest {
            scope_id: Some(scope),
            session_id: None,
            since: None,
            max_observations: 50,
            trigger: ReflectionTrigger::Agent,
        },
    )
    .await
    .unwrap();
    assert!(report.entities_emitted >= 1);
    let derived: Option<Vec<i64>> = sqlx::query_scalar(
        "SELECT derived_from FROM memory_observations
         WHERE source = 'reflection' AND content = 'consolidated'",
    )
    .fetch_optional(pool)
    .await
    .unwrap();
    let derived = derived.expect("derived row");
    assert!(derived.contains(&obs));
}

#[tokio::test(flavor = "multi_thread")]
async fn eval_reflection_records_run_metadata() {
    let db = require_test_db!();
    let pool = db.pool();
    let stats = Arc::new(StatsTracker::new());
    let scope = queries::find_or_create_scope(pool, &ScopeSpec::default())
        .await
        .unwrap();
    let extractor: Arc<dyn LlmExtractor> = Arc::new(CannedExtractor::new());
    run_reflection(
        pool,
        &stats,
        extractor.as_ref(),
        ReflectionRequest {
            scope_id: Some(scope),
            session_id: None,
            since: None,
            max_observations: 50,
            trigger: ReflectionTrigger::Agent,
        },
    )
    .await
    .unwrap();
    let (run_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM memory_reflection_runs WHERE scope_id = $1 AND trigger = 'agent'",
    )
    .bind(scope)
    .fetch_one(pool)
    .await
    .unwrap();
    assert!(run_count >= 1);
}

// =====================================================================
// Sanity: prove the canned extractor compiles end-to-end (one more
// "round-trip" scenario to push the gate past 20).
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn eval_extractor_round_trip_entity_relation_observation() {
    let db = require_test_db!();
    let pool = db.pool();
    let stats = Arc::new(StatsTracker::new());
    let session_id = Uuid::new_v4();
    pgmcp::sessions::upsert_session(pool, session_id, "/ws/eval-extractor", None)
        .await
        .unwrap();
    let sha = pgmcp::sessions::prompt_sha256("round trip");
    let pid = pgmcp::sessions::insert_prompt(pool, session_id, "round trip", &sha, None)
        .await
        .unwrap();
    let mock = Arc::new(CannedExtractor::new());
    *mock.extract.lock().unwrap() = ExtractionResult {
        entities: vec![
            NewEntity {
                name: "eval-extractor-x".into(),
                entity_type: "concept".into(),
                initial_observations: vec!["x".into()],
                importance: 0.7,
            },
            NewEntity {
                name: "eval-extractor-y".into(),
                entity_type: "concept".into(),
                initial_observations: vec![],
                importance: 0.5,
            },
        ],
        relations: vec![NewRelation {
            from_name: "eval-extractor-x".into(),
            to_name: "eval-extractor-y".into(),
            relation_type: "linked".into(),
            importance: 0.6,
        }],
        contradictions: vec![],
    };
    let extractor: Arc<dyn LlmExtractor> = mock as Arc<dyn LlmExtractor>;
    run_extraction_for_prompt(
        pool.clone(),
        stats,
        extractor,
        Arc::new(dashmap::DashMap::new()),
        ExtractorWorkerConfig::default(),
        ExtractorJob {
            session_id,
            source_prompt_id: pid,
            project_id: None,
            agent_id: None,
            user_id: None,
            prompt_text: "round trip".into(),
        },
    )
    .await;
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM memory_entities WHERE name IN ('eval-extractor-x','eval-extractor-y') AND valid_to IS NULL",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(count, 2);
}

// =====================================================================
// MCP-surface coverage (Phase 8 + Phase 10): inventory test requires
// every dispatched tool to be reachable via call_tool_cli from at least
// one integration test. The three tools below close the loop on
// memory_forget, memory_purge_expired, and pgmcp_client_profile.
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn eval_memory_forget_mcp_round_trip() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    let entity_id: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('mcp-forget-target', 'concept', 'agent_write'::memory_source) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("entity");
    let result = server
        .call_tool_cli(
            "memory_forget",
            serde_json::json!({
                "target_type": "entity",
                "target_id": entity_id,
                "cascade": false
            }),
        )
        .await
        .expect("memory_forget");
    let text = result.content[0].raw.as_text().unwrap().text.as_str();
    let body: serde_json::Value = serde_json::from_str(text).expect("json");
    assert_eq!(body.get("cascade").and_then(|v| v.as_bool()), Some(false));
    assert_eq!(
        body.get("target_id").and_then(|v| v.as_i64()),
        Some(entity_id)
    );
    let (active,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM memory_entities WHERE id = $1 AND valid_to IS NULL")
            .bind(entity_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(active, 0, "MCP-driven forget should soft-delete the row");
}

#[tokio::test(flavor = "multi_thread")]
async fn eval_memory_purge_expired_mcp_dry_run() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    let result = server
        .call_tool_cli(
            "memory_purge_expired",
            serde_json::json!({
                "window_days": 90,
                "dry_run": true
            }),
        )
        .await
        .expect("memory_purge_expired");
    let text = result.content[0].raw.as_text().unwrap().text.as_str();
    let body: serde_json::Value = serde_json::from_str(text).expect("json");
    assert_eq!(body.get("dry_run").and_then(|v| v.as_bool()), Some(true));
    let would_delete = body.get("would_delete").expect("would_delete key");
    assert!(would_delete.get("entities").is_some());
    assert!(would_delete.get("observations").is_some());
    assert!(would_delete.get("relations").is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn eval_pgmcp_client_profile_mcp_round_trip() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    let result = server
        .call_tool_cli(
            "pgmcp_client_profile",
            serde_json::json!({"client_name": "claude-code"}),
        )
        .await
        .expect("pgmcp_client_profile");
    let text = result.content[0].raw.as_text().unwrap().text.as_str();
    assert!(text.contains("claude-code"));

    let list = server
        .call_tool_cli(
            "pgmcp_client_profile",
            serde_json::json!({"list_all": true}),
        )
        .await
        .expect("pgmcp_client_profile list_all");
    let list_text = list.content[0].raw.as_text().unwrap().text.as_str();
    let list_body: serde_json::Value = serde_json::from_str(list_text).expect("json");
    let count = list_body.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
    assert!(count >= 3, "expected ≥3 built-in profiles, got {count}");
}
