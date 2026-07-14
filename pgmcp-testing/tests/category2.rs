//! Integration tests for the categorical constructions (ADR-028, item 4):
//! common_dependency (pullback), integration_point (pushout), functorial_impact.
//! Drives all three dispatched tools via call_tool_cli (Layer-D coverage gate).

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

fn body(r: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&text_of(r)).expect("tool body must be JSON")
}

async fn proj(pool: &sqlx::PgPool, name: &str) -> i32 {
    sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $1, $2)
         ON CONFLICT (path) DO UPDATE SET name = $2 RETURNING id",
    )
    .bind(format!("/ws/{name}"))
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("project")
}

async fn edge(pool: &sqlx::PgPool, dependent: i32, dependency: i32) {
    sqlx::query(
        "INSERT INTO project_dependencies (dependent_project_id, dependency_project_id, source, confidence)
         VALUES ($1, $2, 'cargo', 1.0)",
    )
    .bind(dependent)
    .bind(dependency)
    .execute(pool)
    .await
    .expect("edge");
}

#[tokio::test]
async fn pullback_and_pushout() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    let a = proj(&pool, "cat2_a").await;
    let b = proj(&pool, "cat2_b").await;
    let common = proj(&pool, "cat2_common").await;
    let integ = proj(&pool, "cat2_integrator").await;
    // a→common, b→common  (common is a shared dependency / pullback)
    edge(&pool, a, common).await;
    edge(&pool, b, common).await;
    // integrator→a, integrator→b  (integrator is a shared dependent / pushout)
    edge(&pool, integ, a).await;
    edge(&pool, integ, b).await;

    let cd = body(
        &server
            .call_tool_cli(
                "common_dependency",
                json!({"project_a": "cat2_a", "project_b": "cat2_b"}),
            )
            .await
            .expect("common_dependency"),
    );
    let deps: Vec<&str> = cd["common_dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(deps.contains(&"cat2_common"), "pullback: {cd}");

    let ip = body(
        &server
            .call_tool_cli(
                "integration_point",
                json!({"project_a": "cat2_a", "project_b": "cat2_b"}),
            )
            .await
            .expect("integration_point"),
    );
    let ints: Vec<&str> = ip["integration_points"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(ints.contains(&"cat2_integrator"), "pushout: {ip}");
}

#[tokio::test]
async fn functorial_impact_flags_weighting_gap() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    let big = proj(&pool, "fi_big").await;
    let small = proj(&pool, "fi_small").await;
    for (pid, fc, inst) in [(big, 100_i32, 0.2_f64), (small, 10, 0.8)] {
        sqlx::query(
            "INSERT INTO project_metrics (project_id, file_count, avg_instability)
             VALUES ($1, $2, $3) ON CONFLICT (project_id) DO UPDATE SET file_count=$2, avg_instability=$3",
        )
        .bind(pid)
        .bind(fc)
        .bind(inst)
        .execute(&pool)
        .await
        .expect("project_metrics");
    }
    let gid: i64 = sqlx::query_scalar(
        "INSERT INTO project_groups (kind, group_key, label) VALUES ('manual','fi-grp','figrp')
         ON CONFLICT (kind, group_key) DO UPDATE SET label='figrp' RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("group");
    for pid in [big, small] {
        sqlx::query(
            "INSERT INTO project_group_members (group_id, project_id, role) VALUES ($1,$2,'member')
             ON CONFLICT (group_id, project_id) WHERE valid_to IS NULL DO NOTHING",
        )
        .bind(gid)
        .bind(pid)
        .execute(&pool)
        .await
        .expect("member");
    }
    // Unweighted group mean instability = (0.2+0.8)/2 = 0.5.
    sqlx::query(
        "INSERT INTO hier_group_metrics (level, ref_id, label, avg_instability)
         VALUES ('group', $1, 'figrp', 0.5)",
    )
    .bind(gid)
    .execute(&pool)
    .await
    .expect("hier_group_metrics");

    let res = body(
        &server
            .call_tool_cli("functorial_impact", json!({}))
            .await
            .expect("functorial_impact"),
    );
    let impacts = res["impacts"].as_array().unwrap();
    let g = impacts
        .iter()
        .find(|i| i["group_id"].as_i64() == Some(gid))
        .expect("our group present");
    // weighted = (0.2*100 + 0.8*10)/110 ≈ 0.2545; gap ≈ 0.245 > 0.
    assert!(
        g["abs_gap"].as_f64().unwrap() > 0.1,
        "weighting gap detected: {g}"
    );
}
