//! Integration round-trip for the Thread 5b experiment-API hardening MCP tools.
//!
//! Exercises the four new tools end-to-end against real Postgres, plus the
//! `experiment_get` audit-visibility expansion and the decide-time
//! anti-tamper gate:
//!   - `experiment_record_paired_binary_counts` — paired-corpus 2×2 + the
//!     SERVER-COMPUTED McNemar verdict (the agent never asserts it).
//!   - `experiment_finalize_run` — tamper-evident samples digest + finalize +
//!     idempotency.
//!   - `experiment_set_run_status` — audited exclusion (invalid/superseded);
//!     reason required; the anti-cherry-pick re-open cascade.
//!   - `experiment_record_measurement_from_artifact` — server-side CSV / JSONL
//!     parse + path-traversal rejection.
//!   - decide-gate — `experiment_decide` consumes ONLY usable runs.
//!   - `experiment_get` — `measurement_runs` + `paired_binary` sections.
//!
//! Self-skips (via `require_test_db!`) when `PGMCP_TEST_DATABASE_URL` is unset,
//! so it stays green for contributors without a local Postgres+pgvector — while
//! still satisfying `query_inventory_vs_coverage` (which greps these source
//! files for a `call_tool_cli("<tool>", …)` per dispatched tool).
//!
//! EXPERIMENT subsystem only: nothing here touches the work-item tracker (the
//! self-verification loophole was reverted 2026-06-20).

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
/// experiment tables' `vector(1024)` columns).
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

/// Monotonic-ish unique suffix so concurrently-run tests never collide on slug.
fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos()
}

/// Open a latency-optimization experiment and return (slug, experiment_id,
/// hypothesis_id).
async fn open_experiment(server: &McpServer, slug: &str) -> (String, i64, i64) {
    let open = server
        .call_tool_cli(
            "experiment_open",
            json!({
                "title": format!("Hardening fixture {slug}"),
                "question": "Does the change reduce latency?",
                "context": "Validation fixture for Thread 5b.",
                "kind": "optimization",
                "hypothesis": "The change lowers latency_ms",
                "primary_metric": "latency_ms",
                "unit": "ms",
                "lower_is_better": true,
                "slug": slug,
            }),
        )
        .await
        .expect("experiment_open must succeed");
    let ov: Value = serde_json::from_str(&text_of(&open)).expect("open body JSON");
    (
        ov["slug"].as_str().expect("slug").to_string(),
        ov["experiment_id"].as_i64().expect("experiment_id"),
        ov["hypothesis_id"].as_i64().expect("hypothesis_id"),
    )
}

async fn record(
    server: &McpServer,
    experiment_id: i64,
    hypothesis_id: i64,
    arm_label: &str,
    arm_kind: &str,
    samples: &[f64],
) {
    server
        .call_tool_cli(
            "experiment_record_measurement",
            json!({
                "experiment_id": experiment_id,
                "hypothesis_id": hypothesis_id,
                "arm_label": arm_label,
                "arm_kind": arm_kind,
                "metric": "latency_ms",
                "samples": samples,
                "source": "agent_scalar",
            }),
        )
        .await
        .unwrap_or_else(|e| panic!("record {arm_label} must succeed: {e}"));
}

// ============================================================================
// experiment_record_paired_binary_counts
// ============================================================================

#[tokio::test]
async fn paired_binary_counts_compute_mcnemar_server_side() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());
    let slug = format!("hardening-paired-{}", unique_suffix());
    let (slug, experiment_id, hypothesis_id) = open_experiment(&server, &slug).await;

    // b ≫ c (control_only ≫ treatment_only) → strong, significant imbalance.
    let resp = server
        .call_tool_cli(
            "experiment_record_paired_binary_counts",
            json!({
                "experiment_slug": slug,
                "hypothesis_id": hypothesis_id,
                "metric": "answer_correct",
                "both_correct": 40,
                "control_only": 30,
                "treatment_only": 2,
                "both_wrong": 5,
                "source": "external_benchmark",
            }),
        )
        .await
        .expect("record paired binary counts must succeed");
    let v: Value = serde_json::from_str(&text_of(&resp)).expect("paired body JSON");

    let p = v["mcnemar"]["p_value"]
        .as_f64()
        .expect("server-computed p_value present");
    assert!(
        p < 0.05,
        "b≫c must yield a small McNemar p-value (server-computed); got {p}"
    );
    assert_eq!(
        v["significant_at_alpha_0_05"].as_bool(),
        Some(true),
        "verdict must report significance at α=0.05"
    );
    assert_eq!(
        v["mcnemar"]["n_discordant"].as_u64(),
        Some(32),
        "discordant = b + c = 30 + 2"
    );
    assert!(
        v["mcnemar"]["effect_treatment_minus_control"]
            .as_f64()
            .expect("effect")
            < 0.0,
        "c - b < 0: treatment fixed fewer than it broke"
    );
    let paired_id = v["paired_binary_id"].as_i64().expect("paired_binary_id");

    // A row exists in experiment_paired_binary.
    let row: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT both_correct, control_only, treatment_only, both_wrong
         FROM experiment_paired_binary WHERE id = $1",
    )
    .bind(paired_id)
    .fetch_one(db.pool())
    .await
    .expect("stored paired binary row");
    assert_eq!(row, (40, 30, 2, 5), "counts persisted verbatim");

    // Idempotent upsert on (experiment, hypothesis, metric): a second call updates
    // the same row, never inserts a duplicate.
    let resp2 = server
        .call_tool_cli(
            "experiment_record_paired_binary_counts",
            json!({
                "experiment_slug": slug,
                "hypothesis_id": hypothesis_id,
                "metric": "answer_correct",
                "both_correct": 41,
                "control_only": 10,
                "treatment_only": 10,
                "both_wrong": 5,
            }),
        )
        .await
        .expect("upsert paired binary counts must succeed");
    let v2: Value = serde_json::from_str(&text_of(&resp2)).expect("paired body JSON 2");
    assert_eq!(
        v2["paired_binary_id"].as_i64(),
        Some(paired_id),
        "upsert dedupes on (experiment, hypothesis, metric)"
    );
    // b == c now → not significant.
    assert_eq!(
        v2["significant_at_alpha_0_05"].as_bool(),
        Some(false),
        "balanced discordant counts are not significant"
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM experiment_paired_binary WHERE experiment_id = $1",
    )
    .bind(experiment_id)
    .fetch_one(db.pool())
    .await
    .expect("count paired rows");
    assert_eq!(count, 1, "no duplicate paired-binary row");

    // Negative counts and a missing hypothesis are rejected.
    assert!(
        server
            .call_tool_cli(
                "experiment_record_paired_binary_counts",
                json!({
                    "experiment_slug": slug,
                    "hypothesis_id": hypothesis_id,
                    "metric": "answer_correct",
                    "both_correct": -1,
                    "control_only": 0,
                    "treatment_only": 0,
                    "both_wrong": 0,
                }),
            )
            .await
            .is_err(),
        "negative counts must be rejected"
    );
}

// ============================================================================
// experiment_finalize_run
// ============================================================================

#[tokio::test]
async fn finalize_run_seals_digest_and_is_idempotent() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());
    let slug = format!("hardening-finalize-{}", unique_suffix());
    let (slug, _experiment_id, hypothesis_id) = open_experiment(&server, &slug).await;

    let samples = vec![10.0, 10.5, 9.8, 10.2, 10.1];
    record(
        &server,
        _experiment_id,
        hypothesis_id,
        "control",
        "control",
        &samples,
    )
    .await;

    let resp = server
        .call_tool_cli(
            "experiment_finalize_run",
            json!({
                "experiment_slug": slug,
                "hypothesis_id": hypothesis_id,
                "arm_label": "control",
            }),
        )
        .await
        .expect("finalize run must succeed");
    let v: Value = serde_json::from_str(&text_of(&resp)).expect("finalize body JSON");
    assert_eq!(v["status"].as_str(), Some("finalized"));
    assert_eq!(
        v["sample_count"].as_i64(),
        Some(5),
        "non-warmup sample count"
    );
    let digest = v["samples_digest"]
        .as_str()
        .expect("samples_digest present");
    assert!(
        digest.starts_with("sha256:"),
        "samples digest is a sha256 hex; got {digest}"
    );

    // Idempotent: re-finalize recomputes the same digest + same count.
    let resp2 = server
        .call_tool_cli(
            "experiment_finalize_run",
            json!({
                "experiment_slug": slug,
                "hypothesis_id": hypothesis_id,
                "arm_label": "control",
                "reason": "re-seal",
            }),
        )
        .await
        .expect("re-finalize must succeed");
    let v2: Value = serde_json::from_str(&text_of(&resp2)).expect("finalize body JSON 2");
    assert_eq!(
        v2["samples_digest"].as_str(),
        Some(digest),
        "re-finalize is deterministic (same samples → same digest)"
    );

    // A missing arm rejects with invalid_params (not a 500).
    assert!(
        server
            .call_tool_cli(
                "experiment_finalize_run",
                json!({
                    "experiment_slug": slug,
                    "hypothesis_id": hypothesis_id,
                    "arm_label": "nonexistent-arm",
                }),
            )
            .await
            .is_err(),
        "finalizing a nonexistent run must reject"
    );
}

// ============================================================================
// experiment_set_run_status (+ anti-cherry-pick re-open)
// ============================================================================

#[tokio::test]
async fn set_run_status_excludes_and_reopens_decision() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());
    let slug = format!("hardening-status-{}", unique_suffix());
    let (slug, experiment_id, hypothesis_id) = open_experiment(&server, &slug).await;

    let control = vec![10.0, 10.5, 9.8, 10.2, 10.1, 9.9, 10.3, 10.0, 10.2, 9.95];
    let treatment = vec![8.0, 8.2, 7.9, 8.1, 8.0, 7.8, 8.3, 8.0, 8.15, 7.95];
    record(
        &server,
        experiment_id,
        hypothesis_id,
        "control",
        "control",
        &control,
    )
    .await;
    record(
        &server,
        experiment_id,
        hypothesis_id,
        "treatment",
        "treatment",
        &treatment,
    )
    .await;

    // A decision USES the treatment run.
    let decide = server
        .call_tool_cli(
            "experiment_decide",
            json!({ "hypothesis_id": hypothesis_id }),
        )
        .await
        .expect("decide must succeed");
    let dv: Value = serde_json::from_str(&text_of(&decide)).expect("decide JSON");
    assert_eq!(dv["verdict"].as_str(), Some("accepted"));

    // Empty reason is rejected.
    assert!(
        server
            .call_tool_cli(
                "experiment_set_run_status",
                json!({
                    "experiment_slug": slug,
                    "hypothesis_id": hypothesis_id,
                    "arm_label": "treatment",
                    "status": "invalid",
                    "reason": "   ",
                }),
            )
            .await
            .is_err(),
        "empty reason must be rejected"
    );
    // status="finalized" routed here is rejected (use experiment_finalize_run).
    assert!(
        server
            .call_tool_cli(
                "experiment_set_run_status",
                json!({
                    "experiment_slug": slug,
                    "hypothesis_id": hypothesis_id,
                    "arm_label": "treatment",
                    "status": "finalized",
                    "reason": "x",
                }),
            )
            .await
            .is_err(),
        "finalized is not settable via experiment_set_run_status"
    );
    // status="complete" (a lifecycle state) is rejected.
    assert!(
        server
            .call_tool_cli(
                "experiment_set_run_status",
                json!({
                    "experiment_slug": slug,
                    "hypothesis_id": hypothesis_id,
                    "arm_label": "treatment",
                    "status": "complete",
                    "reason": "x",
                }),
            )
            .await
            .is_err(),
        "complete is not an operator-settable exclusion"
    );

    // Invalidate the treatment run with a reason → status invalid + decision reopened.
    let resp = server
        .call_tool_cli(
            "experiment_set_run_status",
            json!({
                "experiment_slug": slug,
                "hypothesis_id": hypothesis_id,
                "arm_label": "treatment",
                "status": "invalid",
                "reason": "discovered a measurement bug in the treatment arm",
            }),
        )
        .await
        .expect("invalidate run must succeed");
    let v: Value = serde_json::from_str(&text_of(&resp)).expect("status body JSON");
    assert_eq!(v["new_status"].as_str(), Some("invalid"));
    let reopened = v["reopened_decisions"]
        .as_array()
        .expect("reopened_decisions array");
    assert_eq!(
        reopened.len(),
        1,
        "the one decision that used this run reopens"
    );
    assert!(
        v["reopened_note"].is_string(),
        "a re-open note is surfaced (anti-cherry-pick)"
    );

    // The run row is now 'invalid' with the recorded reason.
    let run_status: String = sqlx::query_scalar(
        "SELECT status FROM experiment_runs
         WHERE experiment_id = $1 AND hypothesis_id = $2 AND arm_label = 'treatment'",
    )
    .bind(experiment_id)
    .bind(hypothesis_id)
    .fetch_one(db.pool())
    .await
    .expect("run status");
    assert_eq!(run_status, "invalid");

    // The hypothesis verdict reverted to pending (the decision is re-opened).
    let verdict: String =
        sqlx::query_scalar("SELECT verdict::text FROM experiment_hypotheses WHERE id = $1")
            .bind(hypothesis_id)
            .fetch_one(db.pool())
            .await
            .expect("hypothesis verdict");
    assert_eq!(
        verdict, "pending",
        "excluding a run a decision used reverts its verdict to pending"
    );

    // An audit row was appended referencing the re-opened decision.
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM experiment_run_status_audit a
         JOIN experiment_runs r ON r.id = a.run_id
         WHERE r.experiment_id = $1 AND a.new_status = 'invalid'",
    )
    .bind(experiment_id)
    .fetch_one(db.pool())
    .await
    .expect("audit count");
    assert!(
        audit_count >= 1,
        "the exclusion is recorded on the audit trail"
    );
}

// ============================================================================
// decide-gate: experiment_decide consumes only usable runs
// ============================================================================

#[tokio::test]
async fn decide_excludes_invalidated_run_samples() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());
    let slug = format!("hardening-gate-{}", unique_suffix());
    let (slug, experiment_id, hypothesis_id) = open_experiment(&server, &slug).await;

    let control = vec![10.0, 10.5, 9.8, 10.2, 10.1, 9.9, 10.3, 10.0, 10.2, 9.95];
    let treatment = vec![8.0, 8.2, 7.9, 8.1, 8.0, 7.8, 8.3, 8.0, 8.15, 7.95];
    record(
        &server,
        experiment_id,
        hypothesis_id,
        "control",
        "control",
        &control,
    )
    .await;
    record(
        &server,
        experiment_id,
        hypothesis_id,
        "treatment",
        "treatment",
        &treatment,
    )
    .await;

    // Baseline: with both arms usable, n_treatment = 10.
    let decide = server
        .call_tool_cli(
            "experiment_decide",
            json!({ "hypothesis_id": hypothesis_id }),
        )
        .await
        .expect("decide must succeed");
    let dv: Value = serde_json::from_str(&text_of(&decide)).expect("decide JSON");
    assert_eq!(dv["n_treatment"].as_u64(), Some(10));

    // Invalidate the treatment run.
    server
        .call_tool_cli(
            "experiment_set_run_status",
            json!({
                "experiment_slug": slug,
                "hypothesis_id": hypothesis_id,
                "arm_label": "treatment",
                "status": "invalid",
                "reason": "exclude for the gate test",
            }),
        )
        .await
        .expect("invalidate run must succeed");

    // Re-decide: the invalidated run contributes no samples (anti-tamper gate).
    let decide2 = server
        .call_tool_cli(
            "experiment_decide",
            json!({ "hypothesis_id": hypothesis_id }),
        )
        .await
        .expect("re-decide must succeed");
    let dv2: Value = serde_json::from_str(&text_of(&decide2)).expect("re-decide JSON");
    assert_eq!(
        dv2["n_treatment"].as_u64(),
        Some(0),
        "decide must not see samples from an invalidated run"
    );
    assert_eq!(
        dv2["verdict"].as_str(),
        Some("inconclusive"),
        "with no usable treatment samples the test cannot run"
    );
}

// ============================================================================
// experiment_record_measurement_from_artifact (CSV / JSONL + path safety)
// ============================================================================

#[tokio::test]
async fn artifact_ingest_csv_and_jsonl_with_path_safety() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());
    let slug = format!("hardening-artifact-{}", unique_suffix());
    let (slug, experiment_id, hypothesis_id) = open_experiment(&server, &slug).await;

    // The artifact resolver canonicalizes against the daemon's working directory
    // and requires containment within it. Place the fixtures under `target/`
    // (always inside the repo / working dir) so the canonical path is accepted.
    let cwd = std::env::current_dir().expect("cwd");
    let fixture_dir = cwd
        .join("target")
        .join(format!("artifact-fixture-{}", unique_suffix()));
    std::fs::create_dir_all(&fixture_dir).expect("create fixture dir");

    // ── CSV with an arm_column split + a warmup column ──
    let csv_path = fixture_dir.join("bench.csv");
    std::fs::write(
        &csv_path,
        "arm,case,latency_ms,warmup\n\
         control,a,10.0,false\n\
         control,b,10.5,false\n\
         treatment,a,8.0,false\n\
         treatment,b,8.2,false\n\
         treatment,c,7.9,true\n\
         treatment,d,not_a_number,false\n",
    )
    .expect("write csv");
    let csv_rel = csv_path
        .strip_prefix(&cwd)
        .expect("csv path under cwd")
        .to_string_lossy()
        .into_owned();

    let resp = server
        .call_tool_cli(
            "experiment_record_measurement_from_artifact",
            json!({
                "experiment_slug": slug,
                "hypothesis_id": hypothesis_id,
                "arm_label": "ignored-when-arm-column-present",
                "metric": "latency_ms",
                "artifact_path": csv_rel,
                "format": "csv",
                "value_column": "latency_ms",
                "arm_column": "arm",
                "is_warmup_column": "warmup",
                "unit_key_columns": ["case"],
            }),
        )
        .await
        .expect("csv artifact ingest must succeed");
    let v: Value = serde_json::from_str(&text_of(&resp)).expect("artifact body JSON");
    assert_eq!(
        v["skipped"]["non_numeric_or_empty_value"].as_u64(),
        Some(1),
        "the 'not_a_number' row is skipped + reported"
    );
    let total: u64 = v["total_inserted_samples"].as_u64().expect("total samples");
    assert_eq!(
        total, 4,
        "2 control + 2 steady treatment (warmup goes to its own run)"
    );
    // The control arm got 2 samples; an inserted run carries the arm + count.
    let runs = v["runs"].as_array().expect("runs array");
    assert!(
        runs.iter()
            .any(|r| r["arm"] == "control" && r["inserted_samples"] == 2),
        "control arm has 2 samples; got {runs:?}"
    );

    // Samples landed in experiment_samples under the right arms.
    let control_n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM experiment_samples s
         JOIN experiment_runs r ON r.id = s.run_id
         WHERE r.experiment_id = $1 AND s.arm = 'control' AND NOT s.is_warmup",
    )
    .bind(experiment_id)
    .fetch_one(db.pool())
    .await
    .expect("control sample count");
    assert_eq!(control_n, 2);

    // ── JSONL ingest (single arm) ──
    let jsonl_path = fixture_dir.join("bench.jsonl");
    std::fs::write(
        &jsonl_path,
        "{\"latency_ms\": 9.1, \"case\": \"x\"}\n\
         {\"latency_ms\": 9.3, \"case\": \"y\"}\n\
         {\"latency_ms\": \"oops\", \"case\": \"z\"}\n",
    )
    .expect("write jsonl");
    let jsonl_rel = jsonl_path
        .strip_prefix(&cwd)
        .expect("jsonl path under cwd")
        .to_string_lossy()
        .into_owned();
    let resp2 = server
        .call_tool_cli(
            "experiment_record_measurement_from_artifact",
            json!({
                "experiment_slug": slug,
                "hypothesis_id": hypothesis_id,
                "arm_label": "jsonl_arm",
                "arm_kind": "treatment",
                "metric": "latency_ms",
                "artifact_path": jsonl_rel,
                "format": "jsonl",
                "value_column": "latency_ms",
            }),
        )
        .await
        .expect("jsonl artifact ingest must succeed");
    let v2: Value = serde_json::from_str(&text_of(&resp2)).expect("jsonl body JSON");
    assert_eq!(
        v2["total_inserted_samples"].as_u64(),
        Some(2),
        "two numeric JSONL rows ingested, one non-numeric skipped"
    );

    // ── path traversal is rejected ──
    let escape = server
        .call_tool_cli(
            "experiment_record_measurement_from_artifact",
            json!({
                "experiment_slug": slug,
                "hypothesis_id": hypothesis_id,
                "arm_label": "evil",
                "metric": "latency_ms",
                "artifact_path": "../../../../../../etc/passwd",
                "format": "csv",
                "value_column": "latency_ms",
            }),
        )
        .await;
    assert!(
        escape.is_err(),
        "a path escaping the working directory must be rejected"
    );

    // An absolute path is rejected too.
    let abs = server
        .call_tool_cli(
            "experiment_record_measurement_from_artifact",
            json!({
                "experiment_slug": slug,
                "hypothesis_id": hypothesis_id,
                "arm_label": "evil2",
                "metric": "latency_ms",
                "artifact_path": "/etc/passwd",
                "format": "csv",
                "value_column": "latency_ms",
            }),
        )
        .await;
    assert!(abs.is_err(), "an absolute artifact_path must be rejected");

    // Best-effort cleanup of the fixture dir (ADR-022: capture then clean up).
    let _ = std::fs::remove_dir_all(&fixture_dir);
}

// ============================================================================
// experiment_get audit-visibility expansion
// ============================================================================

#[tokio::test]
async fn experiment_get_surfaces_runs_and_paired_binary() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());
    let slug = format!("hardening-get-{}", unique_suffix());
    let (slug, experiment_id, hypothesis_id) = open_experiment(&server, &slug).await;

    let control = vec![10.0, 10.5, 9.8, 10.2, 10.1];
    let treatment = vec![8.0, 8.2, 7.9, 8.1, 8.0];
    record(
        &server,
        experiment_id,
        hypothesis_id,
        "control",
        "control",
        &control,
    )
    .await;
    record(
        &server,
        experiment_id,
        hypothesis_id,
        "treatment",
        "treatment",
        &treatment,
    )
    .await;
    // Finalize control so a digest + finalized status are visible.
    server
        .call_tool_cli(
            "experiment_finalize_run",
            json!({
                "experiment_slug": slug,
                "hypothesis_id": hypothesis_id,
                "arm_label": "control",
            }),
        )
        .await
        .expect("finalize control");
    // Record a 2×2 on the hypothesis's primary metric so experiment_get surfaces it.
    server
        .call_tool_cli(
            "experiment_record_paired_binary_counts",
            json!({
                "experiment_slug": slug,
                "hypothesis_id": hypothesis_id,
                "metric": "latency_ms",
                "both_correct": 20,
                "control_only": 12,
                "treatment_only": 3,
                "both_wrong": 5,
            }),
        )
        .await
        .expect("record paired binary on primary metric");

    let got = server
        .call_tool_cli("experiment_get", json!({ "experiment_id": experiment_id }))
        .await
        .expect("experiment_get must succeed");
    let gv: Value = serde_json::from_str(&text_of(&got)).expect("get body JSON");

    let runs = gv["measurement_runs"]
        .as_array()
        .expect("measurement_runs present");
    assert_eq!(runs.len(), 2, "both arms appear as runs");
    let control_run = runs
        .iter()
        .find(|r| r["arm_label"] == "control")
        .expect("control run in overview");
    assert_eq!(control_run["status"].as_str(), Some("finalized"));
    assert!(
        control_run["samples_digest"]
            .as_str()
            .is_some_and(|d| d.starts_with("sha256:")),
        "finalized run carries its digest in the audit view"
    );
    assert_eq!(
        control_run["usable_in_decision"].as_bool(),
        Some(true),
        "finalized is usable in a decision"
    );

    let paired = gv["paired_binary"]
        .as_array()
        .expect("paired_binary section present");
    assert_eq!(paired.len(), 1, "the one 2×2 on the primary metric appears");
    assert_eq!(paired[0]["metric"].as_str(), Some("latency_ms"));
    assert_eq!(paired[0]["counts"]["control_only"].as_i64(), Some(12));
    assert!(
        paired[0]["mcnemar"]["p_value"].is_number(),
        "experiment_get recomputes the McNemar verdict server-side"
    );
}
