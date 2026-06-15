//! Real-Postgres integration test for the `cron_history` MCP tool and the
//! `cron_run_history` read queries (ADR-018). Seeds mixed-outcome rows, asserts
//! the per-job rollup + recent list through the tool, then exercises
//! `last_successful_completions` (the restart-survival hot path) and the
//! retention sweep directly. Also satisfies the
//! `every_dispatched_tool_has_an_integration_test` coverage gate via the literal
//! `call_tool_cli("cron_history", …)`.
//!
//! `require_test_db!` skips cleanly when no test DB is configured, so this runs
//! inside `verify.sh` Gate 5 without an `#[ignore]`.

use pgmcp_testing::pool_tool_helpers::server_with_pool;
use pgmcp_testing::require_test_db;

/// Extract the first text block of a tool result as JSON.
fn tool_json(result: &rmcp::model::CallToolResult) -> serde_json::Value {
    let text = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .next()
        .expect("text content present");
    serde_json::from_str(&text).expect("tool output is JSON")
}

#[tokio::test(flavor = "multi_thread")]
async fn cron_history_rollup_recent_and_retention() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    // Seed topic-clustering with 4 runs (newest is a manual ok) + symbol-extraction
    // with 1 ok. Completed-at times are staggered so the rollup's
    // latest-outcome and the recent ordering are deterministic.
    sqlx::query(
        "INSERT INTO cron_run_history
            (job_name, trigger_source, outcome, skip_reason, error_detail, duration_ms, completed_at)
         VALUES
            ('topic-clustering','scheduled','ok',    NULL,      NULL,   1000, now() - interval '10 min'),
            ('topic-clustering','scheduled','failed', NULL,     'boom',    5, now() - interval '8 min'),
            ('topic-clustering','scheduled','skipped','cooldown',NULL,     0, now() - interval '6 min'),
            ('topic-clustering','manual',   'ok',    NULL,      NULL,   1200, now() - interval '2 min'),
            ('symbol-extraction','scheduled','ok',   NULL,      NULL,    800, now() - interval '5 min')",
    )
    .execute(&pool)
    .await
    .expect("seed cron_run_history");

    let server = server_with_pool(pool.clone());

    // Full history: per-job rollup + recent list.
    let res = server
        .call_tool_cli("cron_history", serde_json::json!({}))
        .await
        .expect("cron_history ok");
    let json = tool_json(&res);

    let by_job = json["by_job"].as_array().expect("by_job array");
    let tc = by_job
        .iter()
        .find(|j| j["job"] == "topic-clustering")
        .expect("topic-clustering rollup present");
    assert_eq!(tc["run_count"], 4, "4 topic-clustering runs");
    assert_eq!(tc["ok_count"], 2);
    assert_eq!(tc["fail_count"], 1, "failed counts toward fail");
    assert_eq!(tc["skip_count"], 1);
    assert_eq!(tc["last_outcome"], "ok", "newest run is the manual ok");
    assert!(tc["last_ok"].is_string(), "has a last success");
    assert!(
        tc["next_due"].is_string(),
        "topic-clustering has a mapped interval → next_due computed"
    );
    assert_eq!(
        json["recent"].as_array().expect("recent array").len(),
        5,
        "all 5 seeded runs surface in recent"
    );

    // Job filter + limit.
    let res = server
        .call_tool_cli(
            "cron_history",
            serde_json::json!({ "job": "topic-clustering", "limit": 2 }),
        )
        .await
        .expect("cron_history filtered ok");
    let json = tool_json(&res);
    let recent = json["recent"].as_array().expect("recent array");
    assert_eq!(recent.len(), 2, "limit clamps the recent list");
    assert!(
        recent.iter().all(|r| r["job_name"] == "topic-clustering"),
        "job filter applied"
    );

    // Restart-survival hot path: only the latest OK per job.
    let last: std::collections::HashMap<String, chrono::DateTime<chrono::Utc>> =
        pgmcp::db::queries::last_successful_completions(&pool)
            .await
            .expect("last_successful_completions")
            .into_iter()
            .collect();
    assert!(last.contains_key("topic-clustering"));
    assert!(last.contains_key("symbol-extraction"));
    assert_eq!(last.len(), 2, "one row per job with an ok run");

    // Retention sweep: an aged row is removed at 30d; days=0 is a no-op.
    sqlx::query(
        "INSERT INTO cron_run_history (job_name, outcome, completed_at)
         VALUES ('aged-job', 'ok', now() - interval '40 days')",
    )
    .execute(&pool)
    .await
    .expect("seed aged row");
    let deleted = pgmcp::db::queries::delete_cron_runs_older_than(&pool, 30)
        .await
        .expect("retention sweep");
    assert!(deleted >= 1, "the 40-day-old row is swept at 30d retention");
    let noop = pgmcp::db::queries::delete_cron_runs_older_than(&pool, 0)
        .await
        .expect("retention no-op");
    assert_eq!(noop, 0, "days=0 keeps history forever");
}
