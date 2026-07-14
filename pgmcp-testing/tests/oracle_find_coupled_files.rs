//! Real-Postgres correctness oracle for `find_coupled_files`.
//!
//! Uses the synthetic git-history fixture which plants:
//!   (A, B): Jaccard 1.0
//!   (C, D): Jaccard 0.5
//!   (A, C), (A, D), (B, C), (B, D): Jaccard 0.25 / 0.333
//!   E: never co-changes
//!
//! At threshold 0.4 the tool must return exactly the (A, B) and (C, D)
//! pairs — nothing else qualifies. At threshold 0.0 the tool must
//! return all 6 cross-pairs (every two-file subset of {A, B, C, D}).
//! E never appears regardless of threshold.

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_git_history::seed_git_history;
use pgmcp_testing::require_test_db;

const TOL: f64 = 1e-2;

#[tokio::test]
async fn find_coupled_files_returns_planted_pairs_above_threshold() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_git_history(&pool).await;
    let server = server_with_pool(pool);

    // min_coupling=0.4, min_commits=1 → only (A, B) at 1.0 and (C, D)
    // at 0.5 should survive.
    let result = server
        .call_tool_cli(
            "find_coupled_files",
            serde_json::json!({
                "project": "git-coupled",
                "min_coupling": 0.4,
                "min_commits": 1,
            }),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let pairs = v["coupled_pairs"].as_array().expect("coupled_pairs");
    assert_eq!(
        pairs.len(),
        2,
        "expected exactly 2 pairs above threshold 0.4; got {}\npayload:\n{v}",
        pairs.len()
    );

    let by_pair: std::collections::HashMap<(String, String), f64> = pairs
        .iter()
        .map(|p| {
            let mut a = p["file_a"].as_str().unwrap().to_string();
            let mut b = p["file_b"].as_str().unwrap().to_string();
            if a > b {
                std::mem::swap(&mut a, &mut b);
            }
            let j: f64 = p["jaccard"].as_str().unwrap().parse().expect("parse");
            ((a, b), j)
        })
        .collect();

    let ab = by_pair[&("src/a.rs".into(), "src/b.rs".into())];
    let cd = by_pair[&("src/c.rs".into(), "src/d.rs".into())];
    assert!(
        (ab - 1.0).abs() < TOL,
        "Jaccard(A, B) must be 1.0; got {ab}"
    );
    assert!(
        (cd - 0.5).abs() < TOL,
        "Jaccard(C, D) must be 0.5; got {cd}"
    );
}

#[tokio::test]
async fn find_coupled_files_excludes_uncoupled_file_e() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_git_history(&pool).await;
    let server = server_with_pool(pool);

    // At any threshold, file E must never appear in a pair (it
    // never shares a commit with any other file).
    let result = server
        .call_tool_cli(
            "find_coupled_files",
            serde_json::json!({
                "project": "git-coupled",
                "min_coupling": 0.0,
                "min_commits": 1,
            }),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let pairs = v["coupled_pairs"].as_array().expect("coupled_pairs");
    for p in pairs {
        assert_ne!(
            p["file_a"].as_str(),
            Some("src/e.rs"),
            "E must not appear as file_a in any pair"
        );
        assert_ne!(
            p["file_b"].as_str(),
            Some("src/e.rs"),
            "E must not appear as file_b in any pair"
        );
    }
}

#[tokio::test]
async fn find_coupled_files_normalizes_project_and_bounds() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_git_history(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "find_coupled_files",
            serde_json::json!({
                "project": "  git-coupled  ",
                "min_coupling": -5.0,
                "min_commits": -9,
                "limit": -3,
            }),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["project"].as_str(), Some("git-coupled"));
    assert_eq!(v["min_coupling"].as_f64(), Some(0.0));
    assert_eq!(v["min_commits"].as_i64(), Some(1));
    assert_eq!(v["limit"].as_i64(), Some(1));
    assert_eq!(v["pair_count"].as_i64(), Some(1));
    assert_eq!(
        v["coupled_pairs"].as_array().expect("coupled_pairs").len(),
        1
    );
}

#[tokio::test]
async fn find_coupled_files_rejects_duplicate_project_display_names() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_git_history(&pool).await;
    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind("/ws/other")
        .bind("/ws/other/git-coupled-shadow")
        .bind("git-coupled")
        .execute(&pool)
        .await
        .expect("insert duplicate project display name");
    let server = server_with_pool(pool);

    let err = server
        .call_tool_cli(
            "find_coupled_files",
            serde_json::json!({
                "project": "git-coupled",
                "min_coupling": 0.4,
                "min_commits": 1,
            }),
        )
        .await
        .expect_err("duplicate project display names must fail closed");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("ambiguous project name") || msg.contains("not unique"),
        "error should identify duplicate project name; got {msg}"
    );
}
