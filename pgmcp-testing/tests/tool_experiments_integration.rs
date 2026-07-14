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

use std::sync::Arc;

use crate::common::text_of;
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

async fn open_latency_experiment(server: &McpServer, title: &str) -> (i64, i64) {
    let open = server
        .call_tool_cli(
            "experiment_open",
            json!({
                "title": title,
                "question": "Does the change reduce latency?",
                "context": "Validation fixture.",
                "kind": "optimization",
                "hypothesis": "The change lowers latency_ms",
                "primary_metric": "latency_ms",
                "unit": "ms",
                "lower_is_better": true,
            }),
        )
        .await
        .expect("experiment_open must succeed");
    let ov: Value = serde_json::from_str(&text_of(&open)).expect("open body JSON");
    (
        ov["experiment_id"].as_i64().expect("experiment_id"),
        ov["hypothesis_id"].as_i64().expect("hypothesis_id"),
    )
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
                "title": "  Arena allocation on the dispatch hot path  ",
                "question": "  Does arena allocation reduce dispatch p99 latency?  ",
                "context": "The dispatcher allocates per call.",
                "kind": " optimization ",
                "hypothesis": "  Arena allocation lowers latency_ms  ",
                "primary_metric": " latency_ms ",
                "unit": "ms",
                "predicted_direction": " either ",
                "lower_is_better": true,
                "slug": " arena-dispatch-normalized ",
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
    assert_eq!(ov["slug"].as_str(), Some("arena-dispatch-normalized"));
    assert_eq!(ov["kind"].as_str(), Some("optimization"));

    assert!(
        server
            .call_tool_cli(
                "experiment_open",
                json!({
                    "title": "bad direction",
                    "question": "q",
                    "kind": "optimization",
                    "hypothesis": "h",
                    "primary_metric": "m",
                    "predicted_direction": "sideways",
                }),
            )
            .await
            .is_err(),
        "unknown predicted_direction is rejected before enum casts"
    );
    assert!(
        server
            .call_tool_cli(
                "experiment_open",
                json!({
                    "title": "bad project",
                    "question": "q",
                    "kind": "optimization",
                    "hypothesis": "h",
                    "primary_metric": "m",
                    "project_id": 2147483647,
                }),
            )
            .await
            .is_err(),
        "unknown project_id is rejected before insert"
    );
    assert!(
        server
            .call_tool_cli("experiment_get", json!({}))
            .await
            .is_err(),
        "experiment_get requires an id or nonblank slug"
    );
    assert!(
        server
            .call_tool_cli("experiment_get", json!({ "slug": "   " }))
            .await
            .is_err(),
        "blank experiment_get slug is rejected"
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
    let result_count_after_decide: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM experiment_results WHERE hypothesis_id = $1")
            .bind(hypothesis_id)
            .fetch_one(db.pool())
            .await
            .expect("count experiment results after decide");
    assert_eq!(result_count_after_decide, 1);

    let invalid_decide = server
        .call_tool_cli(
            "experiment_decide",
            json!({
                "hypothesis_id": hypothesis_id,
                "control_arm": " same ",
                "treatment_arm": "same"
            }),
        )
        .await
        .expect_err("same control/treatment arm labels must reject");
    assert!(
        invalid_decide
            .to_string()
            .contains("control_arm and treatment_arm must differ"),
        "unexpected invalid decide error: {invalid_decide}"
    );
    let result_count_after_invalid: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM experiment_results WHERE hypothesis_id = $1")
            .bind(hypothesis_id)
            .fetch_one(db.pool())
            .await
            .expect("count experiment results after invalid decide");
    assert_eq!(
        result_count_after_invalid, result_count_after_decide,
        "invalid decide request must not append a result row"
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
    let got_by_slug = server
        .call_tool_cli(
            "experiment_get",
            json!({ "slug": " arena-dispatch-normalized " }),
        )
        .await
        .expect("experiment_get by trimmed slug must succeed");
    assert_eq!(
        serde_json::from_str::<Value>(&text_of(&got_by_slug)).unwrap()["experiment_id"].as_i64(),
        Some(experiment_id),
        "slug lookup trims the caller input"
    );
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
    let artifact = server
        .call_tool_cli(
            "experiment_log_artifact",
            json!({
                "experiment_id": experiment_id,
                "kind": " hyperfine ",
                "tool": "hyperfine",
                "label": "latency benchmark",
                "content": r#"{"results":[{"command":"x","times":[0.0081,0.0079,0.0080]}]}"#,
                "parse": true,
            }),
        )
        .await
        .expect("experiment_log_artifact must succeed");
    let artifact_v: Value = serde_json::from_str(&text_of(&artifact)).expect("artifact body JSON");
    let artifact_id = artifact_v["artifact_id"].as_i64().expect("artifact_id");
    assert_eq!(
        artifact_v["kind"].as_str(),
        Some("hyperfine"),
        "artifact kind is trimmed before parser dispatch and response"
    );
    assert_eq!(
        artifact_v["parsed_sample_count"].as_u64(),
        Some(3),
        "trimmed hyperfine kind still enables auto-parse"
    );
    assert_eq!(artifact_v["parsed_metrics"]["n"].as_u64(), Some(3));
    let stored: (String, Value, Option<String>) = sqlx::query_as(
        "SELECT kind, metrics, content_sha256 FROM experiment_artifacts WHERE id = $1",
    )
    .bind(artifact_id)
    .fetch_one(db.pool())
    .await
    .expect("stored artifact");
    assert_eq!(stored.0, "hyperfine", "stored artifact kind is normalized");
    assert_eq!(stored.1["n"].as_u64(), Some(3));
    assert!(
        stored.2.as_deref().is_some_and(|sha| sha.len() == 64),
        "content_sha256 is stored when content is supplied"
    );

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

#[tokio::test]
async fn experiment_record_measurement_rejects_invalid_inputs_and_normalizes_commit() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    let (experiment_id, hypothesis_id) =
        open_latency_experiment(&server, &format!("Measurement validation {suffix}")).await;
    let (other_experiment_id, other_hypothesis_id) =
        open_latency_experiment(&server, &format!("Measurement validation peer {suffix}")).await;

    let empty_arm = server
        .call_tool_cli(
            "experiment_record_measurement",
            json!({
                "experiment_id": experiment_id,
                "hypothesis_id": hypothesis_id,
                "arm_label": "  ",
                "arm_kind": "control",
                "metric": "latency_ms",
                "samples": [1.0],
            }),
        )
        .await;
    assert!(empty_arm.is_err(), "empty arm labels must be rejected");

    let duplicate_unit_keys = server
        .call_tool_cli(
            "experiment_record_measurement",
            json!({
                "experiment_id": experiment_id,
                "hypothesis_id": hypothesis_id,
                "arm_label": "paired",
                "arm_kind": "control",
                "metric": "latency_ms",
                "samples": [1.0, 2.0],
                "unit_keys": ["src/a.rs", " src/a.rs "],
            }),
        )
        .await;
    assert!(
        duplicate_unit_keys.is_err(),
        "duplicate normalized unit keys must be rejected"
    );

    let invalid_source = server
        .call_tool_cli(
            "experiment_record_measurement",
            json!({
                "experiment_id": experiment_id,
                "hypothesis_id": hypothesis_id,
                "arm_label": "control",
                "arm_kind": "control",
                "metric": "latency_ms",
                "samples": [1.0],
                "source": "spreadsheet",
            }),
        )
        .await;
    assert!(
        invalid_source.is_err(),
        "measurement source must be from the documented enum"
    );

    let mismatched_hypothesis = server
        .call_tool_cli(
            "experiment_record_measurement",
            json!({
                "experiment_id": experiment_id,
                "hypothesis_id": other_hypothesis_id,
                "arm_label": "control",
                "arm_kind": "control",
                "metric": "latency_ms",
                "samples": [1.0],
            }),
        )
        .await;
    assert!(
        mismatched_hypothesis.is_err(),
        "a hypothesis from another experiment must not be recorded"
    );

    let accepted = server
        .call_tool_cli(
            "experiment_record_measurement",
            json!({
                "experiment_id": experiment_id,
                "hypothesis_id": hypothesis_id,
                "arm_label": " paired ",
                "arm_kind": " control ",
                "metric": " latency_ms ",
                "samples": [1.0, 2.0],
                "unit_keys": [" src/a.rs ", "src/b.rs"],
                "source": " manual ",
            }),
        )
        .await
        .expect("normalized measurement must be accepted");
    let accepted_v: Value = serde_json::from_str(&text_of(&accepted)).expect("record JSON");
    assert_eq!(accepted_v["inserted_samples"].as_u64(), Some(2));

    let run: (String, String, Option<String>) = sqlx::query_as(
        "SELECT arm_label, arm_kind::text, runner
         FROM experiment_runs
         WHERE experiment_id = $1 AND hypothesis_id = $2 AND arm_label = 'paired'",
    )
    .bind(experiment_id)
    .bind(hypothesis_id)
    .fetch_one(db.pool())
    .await
    .expect("normalized run row");
    assert_eq!(run.0, "paired");
    assert_eq!(run.1, "control");
    assert_eq!(run.2.as_deref(), Some("manual"));

    let stored: (i64, i64) = sqlx::query_as(
        "SELECT
            COUNT(*) FILTER (WHERE s.metric_name = 'latency_ms')::bigint,
            COUNT(*) FILTER (WHERE s.unit_key IN ('src/a.rs', 'src/b.rs'))::bigint
         FROM experiment_samples s
         JOIN experiment_runs r ON r.id = s.run_id
         WHERE r.experiment_id IN ($1, $2)",
    )
    .bind(experiment_id)
    .bind(other_experiment_id)
    .fetch_one(db.pool())
    .await
    .expect("stored sample counts");
    assert_eq!(
        stored,
        (2, 2),
        "only the accepted normalized submission should reach experiment_samples"
    );
}
