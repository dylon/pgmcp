//! Phase 4 (LLM extractor) + Phase 5 (reflection) integration tests.
//!
//! Uses a deterministic `MockExtractor` so the tests run without
//! downloading Qwen3 weights or talking to the Anthropic API. The real
//! backends are exercised in their respective unit-test modules
//! (`src/llm/cloud.rs::tests`, `src/llm/prompt.rs::tests`,
//! `src/llm/qwen3.rs::tests`).

use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Result;
use pgmcp::db::queries;
use pgmcp::llm::extractor_worker::{
    DebounceMap, ExtractorJob, ExtractorWorkerConfig, run_extraction_for_prompt,
};
use pgmcp::llm::reflect::{ReflectionRequest, ReflectionTrigger, run_reflection};
use pgmcp::llm::{
    ContradictionKind, ContradictionSignal, ExtractionRequest, ExtractionResult, LlmExtractor,
    NewEntity, NewRelation,
};
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::require_test_db;
use uuid::Uuid;

/// Deterministic LlmExtractor used by the integration tests. Caller
/// programs the canned `extract` and `reflect` outputs upfront; the
/// extractor returns them on the first call (and clones them on
/// subsequent calls, so re-entrant tests don't go stale).
struct MockExtractor {
    canned_extract: Mutex<ExtractionResult>,
    canned_reflect: Mutex<Vec<NewEntity>>,
}

impl MockExtractor {
    fn new() -> Self {
        Self {
            canned_extract: Mutex::new(ExtractionResult::default()),
            canned_reflect: Mutex::new(Vec::new()),
        }
    }
    fn set_extract(&self, r: ExtractionResult) {
        *self.canned_extract.lock().unwrap() = r;
    }
    fn set_reflect(&self, r: Vec<NewEntity>) {
        *self.canned_reflect.lock().unwrap() = r;
    }
}

impl LlmExtractor for MockExtractor {
    fn name(&self) -> &'static str {
        "mock"
    }
    fn model_signature(&self) -> &'static str {
        "mock-extractor-v1"
    }
    fn extract(&self, _request: ExtractionRequest<'_>) -> Result<ExtractionResult> {
        Ok(self.canned_extract.lock().unwrap().clone())
    }
    fn reflect(&self, _observations: &[String]) -> Result<Vec<NewEntity>> {
        Ok(self.canned_reflect.lock().unwrap().clone())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn extractor_worker_writes_entities_relations_observations() {
    let db = require_test_db!();
    let pool = db.pool();
    let stats = Arc::new(StatsTracker::new());

    let session_id = Uuid::new_v4();
    let cwd = "/ws/phase4-extract";
    pgmcp::sessions::upsert_session(pool, session_id, cwd, None)
        .await
        .expect("upsert_session");
    let prompt_text =
        "From now on, use Tokio for all async work. Tokio is great because it has work-stealing.";
    let sha = pgmcp::sessions::prompt_sha256(prompt_text);
    let prompt_id = pgmcp::sessions::insert_prompt(pool, session_id, prompt_text, &sha, None)
        .await
        .expect("insert_prompt");

    let mock = Arc::new(MockExtractor::new());
    mock.set_extract(ExtractionResult {
        entities: vec![
            NewEntity {
                name: "Tokio".into(),
                entity_type: "library".into(),
                initial_observations: vec!["work-stealing scheduler".into()],
                importance: 0.8,
            },
            NewEntity {
                name: "async-work-policy".into(),
                entity_type: "preference".into(),
                initial_observations: vec!["use Tokio for all async work".into()],
                importance: 0.9,
            },
        ],
        relations: vec![NewRelation {
            from_name: "async-work-policy".into(),
            to_name: "Tokio".into(),
            relation_type: "mandates".into(),
            importance: 0.9,
        }],
        contradictions: vec![],
    });

    let extractor: Arc<dyn LlmExtractor> = mock as Arc<dyn LlmExtractor>;
    let debounce: DebounceMap = Arc::new(dashmap::DashMap::new());
    let config = ExtractorWorkerConfig::default();
    run_extraction_for_prompt(
        pool.clone(),
        Arc::clone(&stats),
        extractor,
        debounce,
        config,
        ExtractorJob {
            session_id,
            source_prompt_id: prompt_id,
            project_id: None,
            agent_id: None,
            user_id: Some("test-user".into()),
            prompt_text: prompt_text.into(),
        },
    )
    .await;

    let entities_written: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM memory_entities WHERE name IN ('Tokio','async-work-policy') AND valid_to IS NULL")
            .fetch_one(pool)
            .await
            .expect("count entities");
    assert_eq!(entities_written, 2, "both entities should be present");

    let observations_written: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM memory_observations WHERE source = 'llm_extraction' AND valid_to IS NULL")
            .fetch_one(pool)
            .await
            .expect("count obs");
    assert!(
        observations_written >= 2,
        "at least 2 observations should be written, got {}",
        observations_written
    );

    let rel_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memory_relations r
         JOIN memory_entities a ON a.id = r.from_entity_id
         JOIN memory_entities b ON b.id = r.to_entity_id
         WHERE a.name = 'async-work-policy' AND b.name = 'Tokio'
           AND r.relation_type = 'mandates' AND r.valid_to IS NULL",
    )
    .fetch_one(pool)
    .await
    .expect("count relations");
    assert_eq!(rel_count, 1, "expected the mandates relation");

    let runs = stats
        .memory_extractor_runs
        .load(std::sync::atomic::Ordering::Acquire);
    assert!(runs >= 1, "stats counter increment");
}

#[tokio::test(flavor = "multi_thread")]
async fn extractor_worker_invalidates_contradicted_observation() {
    let db = require_test_db!();
    let pool = db.pool();
    let stats = Arc::new(StatsTracker::new());

    // Seed an existing entity + observation we'll contradict.
    let entity_id: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('async-runtime', 'preference', 'agent_write'::memory_source)
         RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("entity");
    let obs_id: i64 = sqlx::query_scalar(
        "INSERT INTO memory_observations (entity_id, content, content_sha256, source)
         VALUES ($1, 'use async-std for everything',
                 '11111111111111111111111111111111111111111111111111111111111111aa',
                 'agent_write'::memory_source)
         RETURNING id",
    )
    .bind(entity_id)
    .fetch_one(pool)
    .await
    .expect("obs");

    // Run the worker with a canned ExtractionResult that flags obs_id as
    // contradicted.
    let session_id = Uuid::new_v4();
    let cwd = "/ws/phase4-contradict";
    pgmcp::sessions::upsert_session(pool, session_id, cwd, None)
        .await
        .expect("upsert_session");
    let sha = pgmcp::sessions::prompt_sha256("override async-std with Tokio");
    let prompt_id = pgmcp::sessions::insert_prompt(pool, session_id, "override", &sha, None)
        .await
        .expect("insert_prompt");

    let mock = Arc::new(MockExtractor::new());
    mock.set_extract(ExtractionResult {
        entities: vec![],
        relations: vec![],
        contradictions: vec![ContradictionSignal {
            conflicting_with: obs_id,
            kind: ContradictionKind::Observation,
            reason: "user switched runtimes".into(),
        }],
    });
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
            prompt_text: "override".into(),
        },
    )
    .await;

    let valid_to: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT valid_to FROM memory_observations WHERE id = $1")
            .bind(obs_id)
            .fetch_one(pool)
            .await
            .expect("read valid_to");
    assert!(
        valid_to.is_some(),
        "contradiction should soft-delete the prior observation"
    );
    let resolved = stats
        .memory_extractor_contradictions_resolved
        .load(std::sync::atomic::Ordering::Acquire);
    assert!(resolved >= 1, "contradictions_resolved counter advanced");
}

#[tokio::test(flavor = "multi_thread")]
async fn run_reflection_writes_derived_from_provenance() {
    let db = require_test_db!();
    let pool = db.pool();
    let stats = Arc::new(StatsTracker::new());

    // Seed an entity + two observations to reflect over.
    let entity_id: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('coding-style', 'preference', 'agent_write'::memory_source)
         RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("entity");
    let obs1: i64 = sqlx::query_scalar(
        "INSERT INTO memory_observations (entity_id, content, content_sha256, source)
         VALUES ($1, 'prefers small focused diffs',
                 'aa11111111111111111111111111111111111111111111111111111111111111',
                 'agent_write'::memory_source) RETURNING id",
    )
    .bind(entity_id)
    .fetch_one(pool)
    .await
    .expect("o1");
    let obs2: i64 = sqlx::query_scalar(
        "INSERT INTO memory_observations (entity_id, content, content_sha256, source)
         VALUES ($1, 'rejects multi-feature PRs',
                 'bb22222222222222222222222222222222222222222222222222222222222222',
                 'agent_write'::memory_source) RETURNING id",
    )
    .bind(entity_id)
    .fetch_one(pool)
    .await
    .expect("o2");

    // Attach the entity to a NULL-scope row so the reflection windowing
    // finds it.
    let scope_id = queries::find_or_create_scope(pool, &queries::ScopeSpec::default())
        .await
        .expect("scope");
    sqlx::query(
        "INSERT INTO memory_entity_scope (entity_id, scope_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(entity_id)
    .bind(scope_id)
    .execute(pool)
    .await
    .expect("attach scope");

    let mock = Arc::new(MockExtractor::new());
    mock.set_reflect(vec![NewEntity {
        name: "diff-discipline".into(),
        entity_type: "summary".into(),
        initial_observations: vec!["consistently prefers small focused diffs".into()],
        importance: 0.75,
    }]);
    let extractor: Arc<dyn LlmExtractor> = mock as Arc<dyn LlmExtractor>;

    let report = run_reflection(
        pool,
        &stats,
        extractor.as_ref(),
        ReflectionRequest {
            scope_id: Some(scope_id),
            session_id: None,
            since: None,
            max_observations: 50,
            trigger: ReflectionTrigger::Agent,
        },
    )
    .await
    .expect("run_reflection");

    assert!(report.observations_considered >= 2);
    assert_eq!(report.entities_emitted, 1);

    // derived_from on the new observation must contain both source ids.
    let derived: Option<Vec<i64>> = sqlx::query_scalar(
        "SELECT derived_from FROM memory_observations
         WHERE source = 'reflection' AND content = 'consistently prefers small focused diffs'",
    )
    .fetch_optional(pool)
    .await
    .expect("derived");
    let derived = derived.expect("the reflection observation should exist");
    assert!(
        derived.contains(&obs1) && derived.contains(&obs2),
        "derived_from must include both source observation ids; got {:?}",
        derived
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_reflect_tool_refuses_when_extractor_disabled() {
    let db = require_test_db!();
    let pool = db.pool();
    // server_with_pool builds a context with llm_extractor=None — exactly
    // the "extractor disabled" state we want to exercise.
    let server = pgmcp_testing::pool_tool_helpers::server_with_pool(pool.clone());
    let result = server
        .call_tool_cli("memory_reflect", serde_json::json!({}))
        .await;
    match result {
        Err(_) => {}
        Ok(r) => assert_eq!(
            r.is_error,
            Some(true),
            "expected error when extractor is disabled, got {r:?}"
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn parse_backend_choice_round_trip() {
    use pgmcp::llm::{LlmBackendChoice, parse_backend_choice};
    assert!(matches!(
        parse_backend_choice("qwen3-8b").unwrap(),
        LlmBackendChoice::Qwen38b
    ));
    assert!(matches!(
        parse_backend_choice("disabled").unwrap(),
        LlmBackendChoice::Disabled
    ));
    assert!(parse_backend_choice("nonsense").is_err());
}
