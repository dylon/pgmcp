//! Integration round-trip for the scientific-experiment MCP tools.
//!
//! Exercises all ten `experiment_*` tools end-to-end against real Postgres:
//! open → protocol → record (control + treatment) → decide → search → get →
//! list → timeline → log_artifact → render_ledger. Self-skips (via
//! `require_test_db!`) when `PGMCP_TEST_DATABASE_URL` is unset, so it stays
//! green for contributors without a local Postgres+pgvector — while still
//! satisfying `query_inventory_vs_coverage` (which greps these source files
//! for a `call_tool_cli("<tool>", …)` per dispatched tool).
//!
//! Uses a local 1024-d deterministic embedder (`server_1024`) because the
//! experiment embedding columns are `vector(1024)` (BGE-M3). The shared
//! `server_with_pool` helper is now also 1024-d (BGE-M3 is the only supported
//! signature), so this local helper is kept for explicitness rather than
//! out of dimensional necessity.

mod common;

use std::sync::Arc;

use arc_swap::ArcSwap;
use common::text_of;
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

/// Server with a real pool and a 1024-d deterministic embedder (matches the
/// experiment tables' `vector(1024)` columns so embed-on-write + vector search
/// don't dimension-mismatch).
fn server_1024(pool: PgPool) -> McpServer {
    let db: Arc<dyn DbClient> = Arc::new(pool);
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(Config::default()));
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

#[tokio::test]
async fn experiment_subsystem_full_round_trip() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());

    // ── open (pre-registers the criterion, returns the prescribed protocol) ──
    let open = server
        .call_tool_cli(
            "experiment_open",
            json!({
                "title": "Arena allocation on the dispatch hot path",
                "question": "Does arena allocation reduce dispatch p99 latency?",
                "context": "The dispatcher allocates per call.",
                "kind": "optimization",
                "hypothesis": "Arena allocation lowers latency_ms",
                "primary_metric": "latency_ms",
                "unit": "ms",
                "lower_is_better": true,
            }),
        )
        .await
        .expect("experiment_open must succeed");
    let ov: Value = serde_json::from_str(&text_of(&open)).expect("open body JSON");
    let experiment_id = ov["experiment_id"].as_i64().expect("experiment_id");
    let hypothesis_id = ov["hypothesis_id"].as_i64().expect("hypothesis_id");
    assert!(
        ov["protocol"].is_object(),
        "open returns a prescribed protocol"
    );
    assert!(
        ov["protocol"]["required_samples_per_arm"].is_number(),
        "stochastic optimization protocol sizes the sample"
    );

    // ── protocol (re-fetch) ──
    server
        .call_tool_cli(
            "experiment_protocol",
            json!({ "experiment_id": experiment_id }),
        )
        .await
        .expect("experiment_protocol must succeed");

    // ── record control (slower) + treatment (faster) ──
    let control = vec![10.0, 10.5, 9.8, 10.2, 10.1, 9.9, 10.3, 10.0, 10.2, 9.95];
    let treatment = vec![8.0, 8.2, 7.9, 8.1, 8.0, 7.8, 8.3, 8.0, 8.15, 7.95];
    server
        .call_tool_cli(
            "experiment_record_measurement",
            json!({
                "experiment_id": experiment_id,
                "hypothesis_id": hypothesis_id,
                "arm_label": "control",
                "arm_kind": "control",
                "metric": "latency_ms",
                "samples": control,
                "source": "agent_scalar",
            }),
        )
        .await
        .expect("record control must succeed");
    server
        .call_tool_cli(
            "experiment_record_measurement",
            json!({
                "experiment_id": experiment_id,
                "hypothesis_id": hypothesis_id,
                "arm_label": "treatment",
                "arm_kind": "treatment",
                "metric": "latency_ms",
                "samples": treatment,
                "source": "agent_scalar",
            }),
        )
        .await
        .expect("record treatment must succeed");

    // ── decide (runs the frozen Welch test) ──
    let decide = server
        .call_tool_cli(
            "experiment_decide",
            json!({ "hypothesis_id": hypothesis_id }),
        )
        .await
        .expect("experiment_decide must succeed");
    let dv: Value = serde_json::from_str(&text_of(&decide)).expect("decide body JSON");
    assert!(dv["verdict"].is_string(), "decide yields a verdict");
    // Treatment is clearly faster (lower) with a large effect → accepted.
    assert_eq!(
        dv["verdict"].as_str(),
        Some("accepted"),
        "a clear, large improvement in the predicted direction should be accepted; got {dv}"
    );

    // ── search (cross-project; 1024-d query vs 1024-d columns) ──
    server
        .call_tool_cli(
            "experiment_search",
            json!({ "query": "arena allocation dispatch latency" }),
        )
        .await
        .expect("experiment_search must succeed");

    // ── get / list / timeline ──
    server
        .call_tool_cli("experiment_get", json!({ "experiment_id": experiment_id }))
        .await
        .expect("experiment_get must succeed");
    server
        .call_tool_cli("experiment_list", json!({ "kind": "optimization" }))
        .await
        .expect("experiment_list must succeed");
    server
        .call_tool_cli(
            "experiment_timeline",
            json!({ "experiment_id": experiment_id }),
        )
        .await
        .expect("experiment_timeline must succeed");

    // ── log_artifact (ad-hoc capture, with hyperfine parse) ──
    server
        .call_tool_cli(
            "experiment_log_artifact",
            json!({
                "experiment_id": experiment_id,
                "kind": "hyperfine",
                "tool": "hyperfine",
                "label": "latency benchmark",
                "content": r#"{"results":[{"command":"x","times":[0.0081,0.0079,0.0080]}]}"#,
                "parse": true,
            }),
        )
        .await
        .expect("experiment_log_artifact must succeed");

    // ── render_ledger (dry-run: returns markdown, writes nothing) ──
    let render = server
        .call_tool_cli(
            "experiment_render_ledger",
            json!({ "experiment_id": experiment_id, "dry_run": true }),
        )
        .await
        .expect("experiment_render_ledger must succeed");
    let rv: Value = serde_json::from_str(&text_of(&render)).expect("render body JSON");
    assert_eq!(
        rv["written"].as_bool(),
        Some(false),
        "dry_run writes nothing"
    );
    let content = rv["content"].as_str().unwrap_or("");
    assert!(
        content.contains("pgmcp_experiment:"),
        "rendered ledger carries the frontmatter slug join-key"
    );
}
