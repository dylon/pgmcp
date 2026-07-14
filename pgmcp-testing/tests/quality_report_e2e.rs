//! End-to-end test for the `quality_report` MCP tool against a real Postgres.
//! Self-skips (via `require_test_db!`) when no test DB is configured.
//!
//! Exercises the full path: CLI dispatch → aggregate (fan-out over ~44
//! collectors + both scorecards) → render. Asserts the three-pillar structure,
//! JSON validity, every rendition target, and clean error handling.

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::require_test_db;

#[tokio::test]
async fn quality_report_grades_three_pillars_and_renders_every_format() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    // ── JSON rendition: structured assertions ───────────────────────────
    let res = server
        .call_tool_cli(
            "quality_report",
            serde_json::json!({"project": "graph-proj", "format": "json"}),
        )
        .await
        .expect("quality_report json call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&res)).expect("valid JSON");

    assert_eq!(v["project"], "graph-proj");
    let pillars = v["pillars"].as_array().expect("pillars array");
    assert_eq!(pillars.len(), 3, "exactly three pillars");
    let names: Vec<&str> = pillars
        .iter()
        .filter_map(|p| p["pillar"].as_str())
        .collect();
    assert!(names.contains(&"Engineering"));
    assert!(names.contains(&"Architecture"));
    assert!(names.contains(&"Security"));
    // At least one pillar must be gradable on the seeded corpus.
    assert!(
        pillars.iter().any(|p| p["grade"].is_string()),
        "at least one pillar should carry a letter grade"
    );
    assert!(v["overall"]["orr_pass"].is_boolean(), "ORR verdict present");
    assert!(v["tool_runs"].as_array().is_some(), "appendix present");

    // ── Every prose rendition is non-empty and names the project ────────
    for fmt in ["markdown", "org", "latex", "html", "text"] {
        let r = server
            .call_tool_cli(
                "quality_report",
                serde_json::json!({"project": "graph-proj", "format": fmt}),
            )
            .await
            .unwrap_or_else(|e| panic!("quality_report {fmt} call failed: {e:?}"));
        let s = text_of(&r);
        assert!(!s.trim().is_empty(), "{fmt} produced empty output");
        assert!(
            s.contains("graph-proj"),
            "{fmt} output missing project name"
        );
    }

    // LaTeX must be a complete, compilable document.
    let tex = text_of(
        &server
            .call_tool_cli(
                "quality_report",
                serde_json::json!({"project": "graph-proj", "format": "latex"}),
            )
            .await
            .expect("latex call"),
    );
    assert!(tex.contains("\\documentclass{article}"));
    assert!(tex.contains("\\end{document}"));

    // ── Bad inputs error cleanly (no silent defaulting) ─────────────────
    assert!(
        server
            .call_tool_cli(
                "quality_report",
                serde_json::json!({"project": "graph-proj", "format": "pdf"}),
            )
            .await
            .is_err(),
        "unknown format must error"
    );
    assert!(
        server
            .call_tool_cli(
                "quality_report",
                serde_json::json!({"project": "graph-proj", "min_severity": "bogus"}),
            )
            .await
            .is_err(),
        "unknown min_severity must error"
    );
}

#[tokio::test]
async fn quality_report_normalizes_project_and_bounds_side_effects() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let h = seed_graph_corpus(&pool).await;

    sqlx::query("DELETE FROM quality_report_history WHERE project_id = $1")
        .bind(h.project_id)
        .execute(&pool)
        .await
        .expect("clear history");
    sqlx::query(
        "INSERT INTO quality_report_history
            (project_id, computed_at, engineering_gpa, architecture_gpa, security_gpa, overall_gpa, raw_summary)
         SELECT $1,
                NOW() - make_interval(secs => g::int),
                2.0,
                2.0,
                2.0,
                2.0,
                '{}'::jsonb
         FROM generate_series(1, 200) AS g",
    )
    .bind(h.project_id)
    .execute(&pool)
    .await
    .expect("seed history");

    let server = server_with_pool(pool.clone());
    let res = server
        .call_tool_cli(
            "quality_report",
            serde_json::json!({
                "project": " graph-proj ",
                "format": " md ",
                "include_underlying_json": true,
                "trend_points": 1000
            }),
        )
        .await
        .expect("quality_report padded project");
    let envelope: serde_json::Value = serde_json::from_str(&text_of(&res)).expect("valid envelope");

    assert_eq!(envelope["format"], "markdown");
    assert_eq!(envelope["report"]["project"], "graph-proj");
    let trend = envelope["report"]["trend"].as_array().expect("trend array");
    assert!(!trend.is_empty(), "trend should include seeded history");
    for pillar in trend {
        let gpas = pillar["gpas"].as_array().expect("gpa array");
        assert!(
            gpas.len() <= 121,
            "trend_points must clamp at 120 persisted samples plus the current run"
        );
    }

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM quality_report_history WHERE project_id = $1")
            .bind(h.project_id)
            .fetch_one(&pool)
            .await
            .expect("history count");
    assert_eq!(
        count, 201,
        "padded project input must persist exactly one history row for the resolved project"
    );

    assert!(
        server
            .call_tool_cli(
                "quality_report",
                serde_json::json!({"project": "graph-proj", "refresh_crons": [""]}),
            )
            .await
            .is_err(),
        "blank refresh_crons entries must reject before cron dispatch"
    );
    assert!(
        server
            .call_tool_cli(
                "quality_report",
                serde_json::json!({
                    "project": "graph-proj",
                    "refresh_crons": [
                        "symbol-extraction",
                        "call-graph",
                        "function-metrics",
                        "graph-analysis",
                        "a2a-reflect",
                        "msm-calibrate",
                        "fuzzy-sync",
                        "symbol-extraction",
                        "call-graph"
                    ]
                }),
            )
            .await
            .is_err(),
        "refresh_crons must be explicitly bounded"
    );
}
