//! Integration test for `project_groups` (ADR-027, item 15 Stage 1): worktree
//! families + singletons derived from git metadata. Drives the dispatched tool
//! through `call_tool_cli` (Layer-D coverage gate).

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

fn body(r: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&text_of(r)).expect("tool body must be JSON")
}

#[tokio::test]
async fn project_groups_detects_worktree_family() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    // Two worktrees of one repo (shared common-dir + root-commits) + a singleton.
    for (path, name, cd, rc) in [
        (
            "/ws/pg/repoA",
            "pgA",
            Some("/ws/pg/repoA/.git"),
            Some("rootcommit1"),
        ),
        (
            "/ws/pg/repoA-feat",
            "pgAfeat",
            Some("/ws/pg/repoA/.git"),
            Some("rootcommit1"),
        ),
        ("/ws/pg/solo", "pgSolo", None, None),
    ] {
        sqlx::query(
            "INSERT INTO projects (workspace_path, path, name, git_common_dir, git_root_commits)
             VALUES ($1, $1, $2, $3, $4)
             ON CONFLICT (path) DO UPDATE SET git_common_dir = $3, git_root_commits = $4",
        )
        .bind(path)
        .bind(name)
        .bind(cd)
        .bind(rc)
        .execute(&pool)
        .await
        .expect("seed project");
    }

    let res = body(
        &server
            .call_tool_cli("project_groups", json!({"rederive": true}))
            .await
            .expect("project_groups"),
    );
    assert!(res["count"].as_i64().unwrap() >= 2, "{res}");
    assert!(
        res["multi_member_groups"].as_i64().unwrap() >= 1,
        "the two repoA worktrees must form one multi-member family: {res}"
    );

    // The family group must carry exactly one `main` (shortest basename = repoA → pgA).
    let groups = res["groups"].as_array().unwrap();
    let family = groups
        .iter()
        .find(|g| {
            g["members"]
                .as_array()
                .map(|m| m.len() > 1)
                .unwrap_or(false)
        })
        .expect("a multi-member family");
    let mains: Vec<&str> = family["members"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "main")
        .map(|m| m["project"].as_str().unwrap())
        .collect();
    assert_eq!(
        mains,
        vec!["pgA"],
        "shortest-basename project is main: {family}"
    );

    // Invalid kind rejected.
    assert!(
        server
            .call_tool_cli("project_groups", json!({"kind": "bogus"}))
            .await
            .is_err()
    );
}
