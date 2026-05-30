//! Phase 1 (trends & forecasting) smoke tests for the `quality_trend` and
//! `quality_forecast` MCP tools.
//!
//! Self-skips (via `require_test_db!`) when `PGMCP_TEST_DATABASE_URL` is unset,
//! so it stays green for contributors without a local Postgres+pgvector — while
//! still satisfying `query_inventory_vs_coverage` (which greps these source
//! files for a `call_tool_cli("<tool>", …)` per dispatched tool).
//!
//! Both tools read `quality_report_history` (populated by the `quality-history`
//! cron). A freshly-seeded project has *no* history rows, so this exercises the
//! graceful empty/short-series path: the tools must return Ok with an
//! "insufficient history" note and null forecast fields rather than erroring.
//! It then seeds two synthetic history rows and re-asserts the trajectory math
//! (a falling overall GPA yields a negative slope and a forward
//! `weeks_to_threshold`).

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
/// other DB-backed smoke harnesses; the dimension is not load-bearing here).
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

/// Idempotently ensure a `projects` row named `name` exists; return its id.
/// The base schema makes `path` the UNIQUE column (not `name`) and requires
/// both `workspace_path` and `path` NOT NULL, so seed those and conflict on
/// `path`.
async fn ensure_project(pool: &PgPool, name: &str) -> i32 {
    let path = format!("/tmp/{name}");
    sqlx::query(
        "INSERT INTO projects (name, workspace_path, path)
         VALUES ($1, $2, $3) ON CONFLICT (path) DO NOTHING",
    )
    .bind(name)
    .bind("/tmp")
    .bind(&path)
    .execute(pool)
    .await
    .expect("seed project row");
    sqlx::query_scalar::<_, i32>("SELECT id FROM projects WHERE path = $1")
        .bind(&path)
        .fetch_one(pool)
        .await
        .expect("project id")
}

#[tokio::test]
async fn quality_trend_and_forecast_empty_history() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    // A project name unlikely to collide with seeded fixtures, so it starts with
    // zero quality_report_history rows.
    let project = "qtrend-empty-smoke";
    ensure_project(&pool, project).await;

    let server = server_1024(pool);

    // ── quality_trend on an empty history: Ok, zero samples, an insufficient-
    //    history note, an empty EWMA line, and null per-pillar deltas. ──
    let trend = server
        .call_tool_cli("quality_trend", json!({ "project": project }))
        .await
        .expect("quality_trend must succeed even with no history");
    let tv: Value = serde_json::from_str(&text_of(&trend)).expect("trend body JSON");
    assert_eq!(tv["project"].as_str(), Some(project));
    assert_eq!(tv["sample_count"].as_i64(), Some(0), "no snapshots yet");
    assert!(
        tv["points"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "points is an empty array"
    );
    assert!(
        tv["note"]
            .as_str()
            .unwrap_or("")
            .contains("insufficient history"),
        "an empty series carries the insufficient-history note"
    );
    assert!(
        tv["delta"]["overall"].is_null(),
        "no delta without two points"
    );

    // ── quality_forecast on an empty history: Ok, null slope/weeks, the
    //    default C-grade threshold, and the insufficient-history note. ──
    let fc = server
        .call_tool_cli("quality_forecast", json!({ "project": project }))
        .await
        .expect("quality_forecast must succeed even with no history");
    let fv: Value = serde_json::from_str(&text_of(&fc)).expect("forecast body JSON");
    assert_eq!(fv["project"].as_str(), Some(project));
    assert!(
        fv["current_overall"].is_null(),
        "no current GPA without history"
    );
    assert!(fv["slope_per_day"].is_null(), "no slope without two points");
    assert!(
        fv["weeks_to_threshold"].is_null(),
        "no projection without a slope"
    );
    assert_eq!(
        fv["threshold"].as_f64(),
        Some(2.0),
        "default threshold is the C-grade floor (2.0)"
    );
    assert!(
        fv["note"]
            .as_str()
            .unwrap_or("")
            .contains("insufficient history"),
        "an empty series carries the insufficient-history note"
    );
}

#[tokio::test]
async fn quality_forecast_projects_a_falling_overall_gpa() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project = "qtrend-falling-smoke";
    let pid = ensure_project(&pool, project).await;

    // Seed two synthetic history rows: overall GPA 3.0 → 2.6 over 10 days. The
    // forecast must report a negative per-day slope and a forward
    // weeks_to_threshold crossing of the 2.5 threshold (between the two points).
    sqlx::query("DELETE FROM quality_report_history WHERE project_id = $1")
        .bind(pid)
        .execute(&pool)
        .await
        .expect("clear prior history");
    sqlx::query(
        "INSERT INTO quality_report_history
            (project_id, engineering_gpa, architecture_gpa, security_gpa, overall_gpa, raw_summary, computed_at)
         VALUES
            ($1, 3.0, 3.0, 3.0, 3.0, '{}'::jsonb, NOW() - INTERVAL '10 days'),
            ($1, 2.6, 2.6, 2.6, 2.6, '{}'::jsonb, NOW())",
    )
    .bind(pid)
    .execute(&pool)
    .await
    .expect("seed two history rows");

    let server = server_1024(pool);

    // ── trend: two samples, a non-null overall delta of -0.4. ──
    let trend = server
        .call_tool_cli("quality_trend", json!({ "project": project, "days": 30 }))
        .await
        .expect("quality_trend must succeed");
    let tv: Value = serde_json::from_str(&text_of(&trend)).expect("trend body JSON");
    assert_eq!(tv["sample_count"].as_i64(), Some(2), "two seeded snapshots");
    let overall_delta = tv["delta"]["overall"]["delta"]
        .as_f64()
        .expect("overall delta present with two points");
    assert!(
        (overall_delta - (-0.4)).abs() < 1e-4,
        "overall GPA fell 0.4 (3.0 → 2.6); got {overall_delta}"
    );
    assert!(
        tv["overall_ewma"]
            .as_array()
            .map(|a| a.len() == 2)
            .unwrap_or(false),
        "EWMA line has one point per sample"
    );

    // ── forecast: negative slope, and a forward crossing of the 2.5 threshold. ──
    let fc = server
        .call_tool_cli(
            "quality_forecast",
            json!({ "project": project, "days": 30, "threshold": 2.5 }),
        )
        .await
        .expect("quality_forecast must succeed");
    let fv: Value = serde_json::from_str(&text_of(&fc)).expect("forecast body JSON");
    assert_eq!(fv["threshold"].as_f64(), Some(2.5));
    let current = fv["current_overall"]
        .as_f64()
        .expect("current overall present");
    assert!(
        (current - 2.6).abs() < 1e-4,
        "current overall is the latest sample (2.6); got {current}"
    );
    let slope = fv["slope_per_day"]
        .as_f64()
        .expect("slope present with two points");
    assert!(
        slope < 0.0,
        "a falling GPA has a negative per-day slope; got {slope}"
    );
    let weeks = fv["weeks_to_threshold"]
        .as_f64()
        .expect("a falling series below-but-near the threshold crosses it");
    assert!(
        weeks > 0.0,
        "weeks_to_threshold is a positive forward projection; got {weeks}"
    );
    assert!(fv["note"].is_null(), "a clean crossing carries no note");
}
