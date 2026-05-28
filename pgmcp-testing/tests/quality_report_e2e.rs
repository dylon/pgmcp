//! End-to-end test for the `quality_report` MCP tool against a real Postgres.
//! Self-skips (via `require_test_db!`) when no test DB is configured.
//!
//! Exercises the full path: CLI dispatch → aggregate (fan-out over ~44
//! collectors + both scorecards) → render. Asserts the three-pillar structure,
//! JSON validity, every rendition target, and clean error handling.

mod common;

use common::{server_with_pool, text_of};
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
