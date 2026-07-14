//! Real-Postgres correctness oracles for `engineering_scorecard`.
//!
//! Per the plan, four tests:
//!
//! 1. `scorecard_perfect_inputs_yields_a_grade` — synthetic corpus
//!    where every dimension scores ≥ 90; assert per-dimension grade
//!    is A and the GPA is 4.0.
//! 2. `scorecard_failing_inputs_yields_f_grade` — corpus where
//!    every dimension scores < 60; assert F across the board and
//!    GPA = 0.0.
//! 3. `scorecard_mixed_inputs_yields_correct_per_dimension_grades`
//!    — verify that dimensions independently reflect their inputs
//!    by toggling one input at a time and asserting only the
//!    targeted dimension changes grade.
//! 4. `scorecard_orr_checklist_reflects_failing_dimensions` — for
//!    each ORR checklist item, seed only the input that gates that
//!    item and assert exactly that one item flips false.

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::{ScorecardScenario, seed_scorecard_corpus};
use pgmcp_testing::require_test_db;

fn dim<'a>(payload: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    payload["dimensions"]
        .as_array()?
        .iter()
        .find(|d| d["dimension"].as_str() == Some(name))
}

#[tokio::test]
async fn scorecard_perfect_inputs_yields_a_grade() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _pid = seed_scorecard_corpus(&pool, "perfect-proj", ScorecardScenario::Perfect).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "engineering_scorecard",
            serde_json::json!({"project": "perfect-proj"}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let dims = v["dimensions"].as_array().expect("dimensions");
    assert_eq!(dims.len(), 10, "10 dimensions");

    for d in dims {
        let g = d["grade"].as_str().unwrap();
        assert_eq!(
            g, "A",
            "dimension {} expected A; got {g}\nfull dim: {d}",
            d["dimension"]
        );
    }

    let gpa: f64 = v["gpa"].as_str().unwrap().parse().expect("parse");
    assert!(
        (gpa - 4.0).abs() < 1e-2,
        "GPA must be 4.0 when every dimension is A; got {gpa}"
    );
    assert_eq!(v["overall_grade"], "A");
    assert_eq!(
        v["orr_pass"].as_bool(),
        Some(true),
        "perfect scorecard must pass ORR"
    );
}

#[tokio::test]
async fn scorecard_failing_inputs_yields_f_grade() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _pid = seed_scorecard_corpus(&pool, "failing-proj", ScorecardScenario::Failing).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "engineering_scorecard",
            serde_json::json!({"project": "failing-proj"}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let dims = v["dimensions"].as_array().expect("dimensions");

    for d in dims {
        let g = d["grade"].as_str().unwrap();
        assert_eq!(
            g, "F",
            "dimension {} expected F on failing corpus; got {g}\nfull dim: {d}",
            d["dimension"]
        );
    }

    let gpa: f64 = v["gpa"].as_str().unwrap().parse().expect("parse");
    assert!(
        gpa.abs() < 1e-2,
        "GPA must be 0.0 when every dimension is F; got {gpa}"
    );
    assert_eq!(v["overall_grade"], "F");
    assert_eq!(
        v["orr_pass"].as_bool(),
        Some(false),
        "failing scorecard must fail ORR"
    );
}

#[tokio::test]
async fn scorecard_mixed_inputs_yields_correct_per_dimension_grades() {
    // Establish independence: toggle one input at a time and assert
    // only the targeted dimension flips out of A.
    let db = require_test_db!();
    let pool = db.pool().clone();

    let _baseline_pid = seed_scorecard_corpus(
        &pool,
        "mixed-baseline",
        ScorecardScenario::OrrFailures {
            cycles: false,
            high_churn: false,
            high_fix: false,
            god_files: false,
            single_author: false,
            stale: false,
            no_docs: false,
            no_tests: false,
        },
    )
    .await;

    let _bus_pid = seed_scorecard_corpus(
        &pool,
        "mixed-bus",
        ScorecardScenario::OrrFailures {
            cycles: false,
            high_churn: false,
            high_fix: false,
            god_files: false,
            single_author: true,
            stale: false,
            no_docs: false,
            no_tests: false,
        },
    )
    .await;

    let _stale_pid = seed_scorecard_corpus(
        &pool,
        "mixed-stale",
        ScorecardScenario::OrrFailures {
            cycles: false,
            high_churn: false,
            high_fix: false,
            god_files: false,
            single_author: false,
            stale: true,
            no_docs: false,
            no_tests: false,
        },
    )
    .await;

    let server = server_with_pool(pool);

    // Baseline: every dimension A.
    let baseline = run_scorecard(&server, "mixed-baseline").await;
    for d in baseline["dimensions"].as_array().unwrap() {
        assert_eq!(
            d["grade"].as_str(),
            Some("A"),
            "baseline dim {} expected A; got {}",
            d["dimension"],
            d["grade"]
        );
    }

    // Toggle 1: single_author only → team_distribution drops, freshness stays A.
    let bus = run_scorecard(&server, "mixed-bus").await;
    let team = dim(&bus, "team_distribution").expect("team_distribution");
    assert_ne!(
        team["grade"].as_str(),
        Some("A"),
        "single_author toggle must drop team_distribution out of A; got {team}"
    );
    let fresh_in_bus = dim(&bus, "freshness").expect("freshness");
    assert_eq!(
        fresh_in_bus["grade"].as_str(),
        Some("A"),
        "freshness must remain A when only single_author is toggled"
    );

    // Toggle 2: stale only → freshness drops, team_distribution stays A.
    let stale = run_scorecard(&server, "mixed-stale").await;
    let fresh = dim(&stale, "freshness").expect("freshness");
    assert_ne!(
        fresh["grade"].as_str(),
        Some("A"),
        "stale toggle must drop freshness out of A; got {fresh}"
    );
    let team_in_stale = dim(&stale, "team_distribution").expect("team_distribution");
    assert_eq!(
        team_in_stale["grade"].as_str(),
        Some("A"),
        "team_distribution must remain A when only stale is toggled"
    );
}

#[tokio::test]
async fn scorecard_orr_checklist_reflects_failing_dimensions() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    // Each scenario flips exactly one ORR-relevant input and asserts
    // exactly that ORR checklist item is false. Mapping (from
    // tool_engineering_scorecard.rs):
    //   no_circular_deps      ← cycle_count == 0
    //   test_coverage         ← test_ratio >= 0.1
    //   has_documentation     ← doc_count > 0
    //   low_churn             ← avg_churn < 3.0
    //   low_fix_ratio         ← avg_fix < 0.3
    //   no_god_files          ← no file exceeds the 2000-line god-file bar
    //   bus_factor_ok         ← avg_authors >= 1.5
    //   recently_maintained   ← avg_stale < 180.0
    let cases: &[(&str, &str, ScorecardScenario)] = &[
        (
            "orr-cycles",
            "no_circular_deps",
            ScorecardScenario::OrrFailures {
                cycles: true,
                high_churn: false,
                high_fix: false,
                god_files: false,
                single_author: false,
                stale: false,
                no_docs: false,
                no_tests: false,
            },
        ),
        (
            "orr-no-tests",
            "test_coverage",
            ScorecardScenario::OrrFailures {
                cycles: false,
                high_churn: false,
                high_fix: false,
                god_files: false,
                single_author: false,
                stale: false,
                no_docs: false,
                no_tests: true,
            },
        ),
        (
            "orr-no-docs",
            "has_documentation",
            ScorecardScenario::OrrFailures {
                cycles: false,
                high_churn: false,
                high_fix: false,
                god_files: false,
                single_author: false,
                stale: false,
                no_docs: true,
                no_tests: false,
            },
        ),
        (
            "orr-churn",
            "low_churn",
            ScorecardScenario::OrrFailures {
                cycles: false,
                high_churn: true,
                high_fix: false,
                god_files: false,
                single_author: false,
                stale: false,
                no_docs: false,
                no_tests: false,
            },
        ),
        (
            "orr-fix",
            "low_fix_ratio",
            ScorecardScenario::OrrFailures {
                cycles: false,
                high_churn: false,
                high_fix: true,
                god_files: false,
                single_author: false,
                stale: false,
                no_docs: false,
                no_tests: false,
            },
        ),
        (
            "orr-god",
            "no_god_files",
            ScorecardScenario::OrrFailures {
                cycles: false,
                high_churn: false,
                high_fix: false,
                god_files: true,
                single_author: false,
                stale: false,
                no_docs: false,
                no_tests: false,
            },
        ),
        (
            "orr-single-author",
            "bus_factor_ok",
            ScorecardScenario::OrrFailures {
                cycles: false,
                high_churn: false,
                high_fix: false,
                god_files: false,
                single_author: true,
                stale: false,
                no_docs: false,
                no_tests: false,
            },
        ),
        (
            "orr-stale",
            "recently_maintained",
            ScorecardScenario::OrrFailures {
                cycles: false,
                high_churn: false,
                high_fix: false,
                god_files: false,
                single_author: false,
                stale: true,
                no_docs: false,
                no_tests: false,
            },
        ),
    ];

    for (proj, _, scenario) in cases {
        let _pid = seed_scorecard_corpus(&pool, proj, *scenario).await;
    }
    let server = server_with_pool(pool);

    for (proj, expected_failing_item, _) in cases {
        let payload = run_scorecard(&server, proj).await;
        let item_value = &payload["orr_checklist"][*expected_failing_item];
        assert_eq!(
            item_value.as_bool(),
            Some(false),
            "scenario `{proj}`: expected `orr_checklist.{expected_failing_item}` = false; payload:\n{payload}"
        );
        assert_eq!(
            payload["orr_pass"].as_bool(),
            Some(false),
            "scenario `{proj}`: any failing item must flip orr_pass to false"
        );
    }
}

#[tokio::test]
async fn scorecard_normalizes_project_and_validates_format() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _pid = seed_scorecard_corpus(&pool, "format-proj", ScorecardScenario::Perfect).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "engineering_scorecard",
            serde_json::json!({"project": " format-proj ", "format": "summary"}),
        )
        .await
        .expect("summary call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["project"].as_str(), Some("format-proj"));
    assert!(
        v["dimensions"]
            .as_array()
            .is_some_and(|dims| dims.is_empty()),
        "summary format omits the per-dimension table"
    );

    assert!(
        server
            .call_tool_cli(
                "engineering_scorecard",
                serde_json::json!({"project": "format-proj", "format": "bogus"}),
            )
            .await
            .is_err(),
        "unknown format must fail closed"
    );
    assert!(
        server
            .call_tool_cli(
                "engineering_scorecard",
                serde_json::json!({"project": "   "}),
            )
            .await
            .is_err(),
        "blank project must fail closed"
    );
}

async fn run_scorecard(server: &pgmcp::mcp::server::McpServer, project: &str) -> serde_json::Value {
    let result = server
        .call_tool_cli(
            "engineering_scorecard",
            serde_json::json!({"project": project}),
        )
        .await
        .expect("scorecard");
    serde_json::from_str(&text_of(&result)).expect("json")
}
