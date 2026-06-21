//! Integration round-trip for the **Crucible P9 pre-registered Context-Tape
//! experiment** MCP tool (`experiment_preregister_context_tape`).
//!
//! Exercises, against real Postgres:
//!   - echo of the FROZEN definition (3 arms, 3 task families, 5 metrics, the
//!     `AllOf`-of-4 composite criterion, the dataset-gated note);
//!   - open → record real passing cells → decide ⇒ `accepted`;
//!   - **promotion default-OFF**: a verified positive decision does NOT write to
//!     memory (`[experiments] allow_promotion=false`, the default);
//!   - **promotion ON + accepted**: bi-temporal supersede of the target memory
//!     observation (prior closed, fresh active successor written);
//!   - **promotion ON + NOT accepted**: a rejected decision still does not
//!     supersede (verified-gated).
//!
//! Self-skips (via `require_test_db!`) when `PGMCP_TEST_DATABASE_URL` is unset,
//! so it stays green for contributors without a local Postgres+pgvector — while
//! still satisfying `query_inventory_vs_coverage` (which greps these source
//! files for a `call_tool_cli("<tool>", …)` per dispatched tool).
//!
//! Uses a 1024-d deterministic embedder because the experiment embedding columns
//! are `vector(1024)` (BGE-M3).

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::db::DbClient;
use pgmcp::embed::EmbedSource;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::McpServer;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;
use serde_json::{Value, json};
use sqlx::PgPool;

/// Server over a real pool with a 1024-d deterministic embedder and an explicit
/// `[experiments] allow_promotion` flag (so the promotion gate can be toggled).
fn server_with_promotion(pool: PgPool, allow_promotion: bool) -> McpServer {
    let db: Arc<dyn DbClient> = Arc::new(pool);
    let stats = Arc::new(StatsTracker::new());
    let mut config_value = Config::default();
    config_value.experiments.allow_promotion = allow_promotion;
    let config = Arc::new(ArcSwap::from_pointee(config_value));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn pgmcp::embed::EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(1024));
    let embed_source = EmbedSource::backend(embed_backend);
    let lifecycle = pgmcp::daemon_state::DaemonLifecycle::new();
    lifecycle.transition(pgmcp::daemon_state::DaemonPhase::Ready);
    let ctx = SystemContext::production(
        db,
        embed_source,
        stats,
        config,
        log_broadcaster,
        task_store,
        lifecycle,
    );
    McpServer::new(ctx)
}

fn text_of(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .next()
        .expect("text content present")
}

/// A complete set of cells (control/treatment/baseline × the gated metrics) that
/// PASSES every clause of the frozen criterion. Real-shaped synthetic numbers
/// exercise the recording + decision path end-to-end; a live run substitutes
/// dataset/model output here.
fn passing_cells() -> Value {
    json!([
        // accuracy (higher better): treatment >> control.
        {"arm":"control","family":"oolong_pairs","metric":"accuracy",
         "samples":[0.60,0.62,0.59,0.61,0.60,0.58,0.63,0.60,0.61,0.59,0.60,0.62]},
        {"arm":"treatment","family":"oolong_pairs","metric":"accuracy",
         "samples":[0.78,0.80,0.77,0.79,0.81,0.76,0.80,0.78,0.79,0.77,0.80,0.79]},
        // cost (lower better): treatment ≈ control (within ±20%).
        {"arm":"control","family":"oolong_pairs","metric":"dollar_cost",
         "samples":[1.00,1.02,0.99,1.01,1.00,0.98,1.01,1.00,1.00,0.99,1.00,1.01]},
        {"arm":"treatment","family":"oolong_pairs","metric":"dollar_cost",
         "samples":[1.02,1.01,1.03,1.00,1.02,0.99,1.01,1.02,1.00,1.01,1.02,1.00]},
        // p95 latency: treatment well under the 30_000 ms SLO.
        {"arm":"treatment","family":"oolong_pairs","metric":"p95_latency_ms",
         "samples":[1000.0,1100.0,1200.0,1050.0,1150.0,1300.0,1250.0,1080.0,1120.0,1090.0,1110.0,1400.0]},
        // max-context: treatment ≥ 2× baseline.
        {"arm":"baseline","family":"oolong_pairs","metric":"max_context_handled",
         "samples":[128000.0,128000.0,128000.0,128000.0]},
        {"arm":"treatment","family":"oolong_pairs","metric":"max_context_handled",
         "samples":[1000000.0,1000000.0,1000000.0,1000000.0]}
    ])
}

/// Same as [`passing_cells`] but treatment accuracy equals control → the
/// accuracy clause (Welch greater) FAILS, so the decision is rejected.
fn failing_cells() -> Value {
    json!([
        {"arm":"control","family":"oolong_pairs","metric":"accuracy",
         "samples":[0.60,0.62,0.59,0.61,0.60,0.58,0.63,0.60,0.61,0.59,0.60,0.62]},
        {"arm":"treatment","family":"oolong_pairs","metric":"accuracy",
         "samples":[0.60,0.62,0.59,0.61,0.60,0.58,0.63,0.60,0.61,0.59,0.60,0.62]},
        {"arm":"control","family":"oolong_pairs","metric":"dollar_cost",
         "samples":[1.00,1.02,0.99,1.01,1.00,0.98,1.01,1.00,1.00,0.99,1.00,1.01]},
        {"arm":"treatment","family":"oolong_pairs","metric":"dollar_cost",
         "samples":[1.02,1.01,1.03,1.00,1.02,0.99,1.01,1.02,1.00,1.01,1.02,1.00]},
        {"arm":"treatment","family":"oolong_pairs","metric":"p95_latency_ms",
         "samples":[1000.0,1100.0,1200.0,1050.0,1150.0,1300.0,1250.0,1080.0,1120.0,1090.0,1110.0,1400.0]},
        {"arm":"baseline","family":"oolong_pairs","metric":"max_context_handled",
         "samples":[128000.0,128000.0,128000.0,128000.0]},
        {"arm":"treatment","family":"oolong_pairs","metric":"max_context_handled",
         "samples":[1000000.0,1000000.0,1000000.0,1000000.0]}
    ])
}

/// Seed a fresh memory entity + one active observation, returning the
/// observation id. Used as the promotion target so each test owns its own row
/// (robust under parallel test execution — no shared-slug interference).
async fn seed_observation(pool: &PgPool, tag: &str) -> i64 {
    let entity_id: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, importance, source)
         VALUES ($1, 'concept', 0.5, 'agent_write') RETURNING id",
    )
    .bind(format!("ctape-promote-target-{tag}"))
    .fetch_one(pool)
    .await
    .expect("insert entity");
    let content = format!("prior content for {tag}");
    let sha = format!("{:064x}", (content.len() as u128) * 1_000_003 + 7);
    sqlx::query_scalar(
        "INSERT INTO memory_observations
            (entity_id, content, content_sha256, importance, source, valid_from)
         VALUES ($1, $2, $3, 0.5, 'agent_write', NOW()) RETURNING id",
    )
    .bind(entity_id)
    .bind(&content)
    .bind(&sha)
    .fetch_one(pool)
    .await
    .expect("insert observation")
}

#[tokio::test]
async fn preregistration_echoes_the_frozen_definition() {
    let db = require_test_db!();
    let server = server_with_promotion(db.pool().clone(), false);

    // No open, no cells — just echo the frozen definition.
    let out = server
        .call_tool_cli("experiment_preregister_context_tape", json!({}))
        .await
        .expect("preregister echo must succeed");
    let v: Value = serde_json::from_str(&text_of(&out)).expect("body JSON");
    let pre = &v["preregistration"];
    assert_eq!(pre["slug"], "crucible-context-tape-3x3x5");
    assert_eq!(pre["arms"].as_array().expect("arms").len(), 3, "3 arms");
    assert_eq!(
        pre["task_families"].as_array().expect("families").len(),
        3,
        "3 task families"
    );
    assert_eq!(
        pre["metrics"].as_array().expect("metrics").len(),
        5,
        "5 metrics"
    );
    assert_eq!(
        pre["clauses"].as_array().expect("clauses").len(),
        4,
        "the composite has 4 clauses"
    );
    assert_eq!(
        pre["frozen_criterion"]["type"], "all_of",
        "the frozen criterion is an AllOf"
    );
    assert!(
        pre["dataset_gated_note"]
            .as_str()
            .unwrap_or_default()
            .contains("OOLONG-Pairs"),
        "the dataset-gated note is present"
    );
    // The closed-vocab SQL forms are surfaced (ADR-003 single source of truth).
    assert!(
        pre["vocab_sql"]["metric"]
            .as_str()
            .unwrap_or_default()
            .contains("'accuracy'"),
        "metric vocab sql_in_list surfaced"
    );
}

#[tokio::test]
async fn open_record_decide_accepts_on_passing_cells() {
    let db = require_test_db!();
    let server = server_with_promotion(db.pool().clone(), false);

    let out = server
        .call_tool_cli(
            "experiment_preregister_context_tape",
            json!({ "open": true, "cells": passing_cells(), "decide": true }),
        )
        .await
        .expect("open+record+decide must succeed");
    let v: Value = serde_json::from_str(&text_of(&out)).expect("body JSON");

    assert!(
        v["opened"]["experiment_id"].is_number(),
        "experiment opened"
    );
    assert_eq!(v["opened"]["criterion_locked"], true, "criterion locked");
    assert!(
        v["recorded"]["cells_recorded"].as_i64().unwrap_or(0) >= 7,
        "all supplied cells recorded"
    );
    // In-memory preview AND persisted decision both accept.
    assert_eq!(v["preview"]["accepted"], true, "in-memory preview accepts");
    assert_eq!(
        v["decision"]["accepted"],
        true,
        "persisted frozen-criterion decision accepts:\n{}",
        serde_json::to_string_pretty(&v["decision"]).unwrap_or_default()
    );
    assert_eq!(
        v["decision"]["clauses"].as_array().expect("clauses").len(),
        4,
        "the decision reports all four clauses"
    );
}

#[tokio::test]
async fn promotion_default_off_does_not_write_memory() {
    let db = require_test_db!();
    // allow_promotion defaults to FALSE.
    let server = server_with_promotion(db.pool().clone(), false);

    // Open + record passing cells (this also mirrors an experiment observation).
    let opened = server
        .call_tool_cli(
            "experiment_preregister_context_tape",
            json!({ "open": true, "cells": passing_cells() }),
        )
        .await
        .expect("open+record must succeed");
    let ov: Value = serde_json::from_str(&text_of(&opened)).expect("open body");
    assert_eq!(ov["decision"]["accepted"], true, "decision is accepted");

    let obs_id = seed_observation(db.pool(), &format!("off-{}", uuid::Uuid::now_v7())).await;
    let hypothesis_id = ov["opened"]["hypothesis_id"].as_i64().expect("hyp id");

    // Ask to promote — but allow_promotion=false, so it MUST refuse.
    let promoted = server
        .call_tool_cli(
            "experiment_preregister_context_tape",
            json!({
                "hypothesis_id": hypothesis_id,
                "experiment_id": ov["opened"]["experiment_id"],
                "decide": true,
                "promote_to_obs": obs_id,
            }),
        )
        .await
        .expect("decide+promote call must succeed");
    let pv: Value = serde_json::from_str(&text_of(&promoted)).expect("promote body");
    assert_eq!(
        pv["decision"]["accepted"], true,
        "still an accepted decision"
    );
    assert_eq!(
        pv["promotion"]["outcome"]["kind"], "disabled",
        "promotion is disabled by default"
    );
    assert_eq!(pv["promotion"]["allow_promotion"], false);

    // The observation is UNCHANGED: still active, no successor written.
    let still_active: bool =
        sqlx::query_scalar("SELECT valid_to IS NULL FROM memory_observations WHERE id = $1")
            .bind(obs_id)
            .fetch_one(db.pool())
            .await
            .expect("obs row");
    assert!(
        still_active,
        "the target observation must remain active (not superseded)"
    );
    let successors: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM memory_observations WHERE $1 = ANY(derived_from)")
            .bind(obs_id)
            .fetch_one(db.pool())
            .await
            .expect("successor count");
    assert_eq!(
        successors, 0,
        "default-OFF must write no superseding observation"
    );
}

#[tokio::test]
async fn promotion_on_and_accepted_supersedes_the_observation() {
    let db = require_test_db!();
    // allow_promotion = TRUE (operator opted in).
    let server = server_with_promotion(db.pool().clone(), true);

    let opened = server
        .call_tool_cli(
            "experiment_preregister_context_tape",
            json!({ "open": true, "cells": passing_cells() }),
        )
        .await
        .expect("open+record must succeed");
    let ov: Value = serde_json::from_str(&text_of(&opened)).expect("open body");
    let obs_id = seed_observation(db.pool(), &format!("on-{}", uuid::Uuid::now_v7())).await;

    let promoted = server
        .call_tool_cli(
            "experiment_preregister_context_tape",
            json!({
                "experiment_id": ov["opened"]["experiment_id"],
                "hypothesis_id": ov["opened"]["hypothesis_id"],
                "decide": true,
                "promote_to_obs": obs_id,
            }),
        )
        .await
        .expect("decide+promote must succeed");
    let pv: Value = serde_json::from_str(&text_of(&promoted)).expect("promote body");
    assert_eq!(pv["decision"]["accepted"], true, "decision accepted");
    assert_eq!(
        pv["promotion"]["outcome"]["kind"], "promoted",
        "an accepted decision with the flag ON promotes"
    );
    let new_id = pv["promotion"]["outcome"]["new_observation_id"]
        .as_i64()
        .expect("new observation id");

    // Bi-temporal supersede: prior is closed, a fresh active successor exists,
    // derived from the prior.
    let prior_closed: bool =
        sqlx::query_scalar("SELECT valid_to IS NOT NULL FROM memory_observations WHERE id = $1")
            .bind(obs_id)
            .fetch_one(db.pool())
            .await
            .expect("prior row");
    assert!(
        prior_closed,
        "the prior observation must be closed (valid_to set)"
    );

    let (active, derived): (bool, bool) = sqlx::query_as(
        "SELECT valid_to IS NULL, $2 = ANY(derived_from)
         FROM memory_observations WHERE id = $1",
    )
    .bind(new_id)
    .bind(obs_id)
    .fetch_one(db.pool())
    .await
    .expect("successor row");
    assert!(active, "the superseding observation must be active");
    assert!(derived, "the successor must be derived_from the prior");
}

#[tokio::test]
async fn promotion_on_but_rejected_does_not_supersede() {
    let db = require_test_db!();
    let server = server_with_promotion(db.pool().clone(), true);

    // Record FAILING cells → the decision is rejected.
    let opened = server
        .call_tool_cli(
            "experiment_preregister_context_tape",
            json!({ "open": true, "cells": failing_cells() }),
        )
        .await
        .expect("open+record must succeed");
    let ov: Value = serde_json::from_str(&text_of(&opened)).expect("open body");
    assert_eq!(
        ov["decision"]["accepted"], false,
        "failing cells must reject the frozen criterion"
    );
    let obs_id = seed_observation(db.pool(), &format!("rej-{}", uuid::Uuid::now_v7())).await;

    let promoted = server
        .call_tool_cli(
            "experiment_preregister_context_tape",
            json!({
                "experiment_id": ov["opened"]["experiment_id"],
                "hypothesis_id": ov["opened"]["hypothesis_id"],
                "decide": true,
                "promote_to_obs": obs_id,
            }),
        )
        .await
        .expect("decide+promote must succeed");
    let pv: Value = serde_json::from_str(&text_of(&promoted)).expect("promote body");
    assert_eq!(pv["decision"]["accepted"], false, "still rejected");
    assert_eq!(
        pv["promotion"]["outcome"]["kind"], "not_accepted",
        "a rejected decision is verified-gated out of promotion even with the flag ON"
    );

    // The observation is untouched.
    let still_active: bool =
        sqlx::query_scalar("SELECT valid_to IS NULL FROM memory_observations WHERE id = $1")
            .bind(obs_id)
            .fetch_one(db.pool())
            .await
            .expect("obs row");
    assert!(
        still_active,
        "a rejected decision must not supersede the observation"
    );
}
