//! Integration test for `workspace_architecture_quality` (ADR-027 Stage 5) over
//! seeded `project_metrics` + grouping, with `rebuild=true` exercising the
//! group/workspace rollup. Drives the dispatched tool via `call_tool_cli`
//! (Layer-D coverage gate).

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

fn body(r: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&text_of(r)).expect("tool body must be JSON")
}

#[tokio::test]
async fn workspace_architecture_quality_rolls_up() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    // Two projects with project_metrics + a group spanning them.
    let mut ids = Vec::new();
    for (name, dist) in [("waq_a", 0.1_f64), ("waq_b", 0.5_f64)] {
        let pid: i32 = sqlx::query_scalar(
            "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $1, $2)
             ON CONFLICT (path) DO UPDATE SET name = $2 RETURNING id",
        )
        .bind(format!("/ws/{name}"))
        .bind(name)
        .fetch_one(&pool)
        .await
        .expect("project");
        sqlx::query(
            "INSERT INTO project_metrics
                (project_id, file_count, module_count, avg_instability, avg_abstractness,
                 avg_distance, architecture_quality_score)
             VALUES ($1, 10, 3, 0.4, 0.2, $2, $3)
             ON CONFLICT (project_id) DO UPDATE SET avg_distance = $2,
                architecture_quality_score = $3",
        )
        .bind(pid)
        .bind(dist)
        .bind(1.0 - dist)
        .execute(&pool)
        .await
        .expect("project_metrics");
        ids.push(pid);
    }
    let gid: i64 = sqlx::query_scalar(
        "INSERT INTO project_groups (kind, group_key, label) VALUES ('manual', 'waq-grp', 'waq')
         ON CONFLICT (kind, group_key) DO UPDATE SET label = 'waq' RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("group");
    for pid in &ids {
        sqlx::query(
            "INSERT INTO project_group_members (group_id, project_id, role) VALUES ($1, $2, 'member')
             ON CONFLICT (group_id, project_id) WHERE valid_to IS NULL DO NOTHING",
        )
        .bind(gid)
        .bind(pid)
        .execute(&pool)
        .await
        .expect("member");
    }

    let res = body(
        &server
            .call_tool_cli("workspace_architecture_quality", json!({"rebuild": true}))
            .await
            .expect("workspace_architecture_quality"),
    );

    // Workspace summary aggregates both projects.
    let ws = &res["workspace"];
    assert!(
        ws["project_count"].as_i64().unwrap() >= 2,
        "workspace must aggregate the seeded projects: {res}"
    );
    // Projects listed worst-quality first → waq_b (dist 0.5, score 0.5) before waq_a.
    let projects = res["projects"].as_array().unwrap();
    let names: Vec<&str> = projects
        .iter()
        .map(|p| p["project"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"waq_a") && names.contains(&"waq_b"),
        "{names:?}"
    );
    let ia = names.iter().position(|n| *n == "waq_a").unwrap();
    let ib = names.iter().position(|n| *n == "waq_b").unwrap();
    assert!(
        ib < ia,
        "worst architecture-quality project comes first: {names:?}"
    );

    // The group summary is present.
    assert!(
        res["groups"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["label"] == "waq"),
        "group summary missing: {res}"
    );
}
