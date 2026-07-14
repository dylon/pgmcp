//! Integration test for `cross_project_coupling` (ADR-027 Stage 4): per-project
//! Ce/Ca + cross-project cycle detection over project_dependencies. Drives the
//! dispatched tool via `call_tool_cli` (Layer-D coverage gate).

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

fn body(r: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&text_of(r)).expect("tool body must be JSON")
}

#[tokio::test]
async fn cross_project_coupling_detects_cycle() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    // Three projects in a dependency cycle: cpA → cpB → cpC → cpA.
    let mut ids = std::collections::HashMap::new();
    for name in ["cpA", "cpB", "cpC"] {
        let pid: i32 = sqlx::query_scalar(
            "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $1, $2)
             ON CONFLICT (path) DO UPDATE SET name = $2 RETURNING id",
        )
        .bind(format!("/ws/{name}"))
        .bind(name)
        .fetch_one(&pool)
        .await
        .expect("project");
        ids.insert(name, pid);
    }
    for (from, to) in [("cpA", "cpB"), ("cpB", "cpC"), ("cpC", "cpA")] {
        sqlx::query(
            "INSERT INTO project_dependencies (dependent_project_id, dependency_project_id, source, confidence)
             VALUES ($1, $2, 'cargo', 1.0)",
        )
        .bind(ids[from])
        .bind(ids[to])
        .execute(&pool)
        .await
        .expect("edge");
    }

    let res = body(
        &server
            .call_tool_cli("cross_project_coupling", json!({}))
            .await
            .expect("cross_project_coupling"),
    );

    assert!(
        res["cross_project_cycle_count"].as_i64().unwrap() >= 1,
        "the 3-project cycle must be detected: {res}"
    );
    // The detected SCC contains all three.
    let cycles = res["cross_project_cycles"].as_array().unwrap();
    let big = cycles
        .iter()
        .find(|c| c.as_array().map(|a| a.len() >= 3).unwrap_or(false))
        .expect("a 3-member cycle");
    let members: Vec<&str> = big
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();
    for p in ["cpA", "cpB", "cpC"] {
        assert!(members.contains(&p), "cycle missing {p}: {members:?}");
    }

    // Each project has Ce=1, Ca=1 → instability 0.5.
    let projects = res["projects"].as_array().unwrap();
    let cpa = projects
        .iter()
        .find(|p| p["project"] == "cpA")
        .expect("cpA");
    assert_eq!(cpa["efferent_coupling"].as_i64(), Some(1));
    assert_eq!(cpa["afferent_coupling"].as_i64(), Some(1));
}
