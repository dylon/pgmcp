//! Integration test for `cross_project_cve_exposure` (ADR-027 E6): a vulnerable
//! dependency in project C propagates exposure to its transitive dependents
//! A → B → C. Drives the dispatched tool via call_tool_cli (coverage gate).

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

fn body(r: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&text_of(r)).expect("tool body must be JSON")
}

#[tokio::test]
async fn cve_exposure_propagates_to_dependents() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    let mut id = std::collections::HashMap::new();
    for n in ["cve_a", "cve_b", "cve_c"] {
        let pid: i32 = sqlx::query_scalar(
            "INSERT INTO projects (workspace_path, path, name) VALUES ($1,$1,$2)
             ON CONFLICT (path) DO UPDATE SET name=$2 RETURNING id",
        )
        .bind(format!("/ws/{n}"))
        .bind(n)
        .fetch_one(&pool)
        .await
        .expect("project");
        id.insert(n, pid);
    }
    // A → B → C (A, B transitively depend on C).
    for (dep, depcy) in [("cve_a", "cve_b"), ("cve_b", "cve_c")] {
        sqlx::query(
            "INSERT INTO project_dependencies (dependent_project_id, dependency_project_id, source, confidence)
             VALUES ($1, $2, 'cargo', 1.0)",
        )
        .bind(id[dep])
        .bind(id[depcy])
        .execute(&pool)
        .await
        .expect("edge");
    }
    // C has a Cargo.toml depending on a vulnerable package.
    sqlx::query(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, modified_at)
         VALUES ($1, '/ws/cve_c/Cargo.toml', 'Cargo.toml', 'toml', 50,
                 '[dependencies]\nbadpkg = \"1.0\"\n', 'cvehash', 2, NOW())
         ON CONFLICT (path) DO UPDATE SET content = EXCLUDED.content",
    )
    .bind(id["cve_c"])
    .execute(&pool)
    .await
    .expect("manifest");
    sqlx::query(
        "INSERT INTO vuln_advisories (advisory_id, ecosystem, package, severity, summary)
         VALUES ('TEST-CVE-1', 'crates.io', 'badpkg', 'high', 'test advisory')
         ON CONFLICT DO NOTHING",
    )
    .execute(&pool)
    .await
    .expect("advisory");

    let res = body(
        &server
            .call_tool_cli("cross_project_cve_exposure", json!({}))
            .await
            .expect("cross_project_cve_exposure"),
    );
    let projects = res["projects"].as_array().expect("projects");
    let by = |name: &str| projects.iter().find(|p| p["project"] == name).cloned();

    let c = by("cve_c").expect("cve_c exposed (direct)");
    assert!(
        c["direct_vulnerable_packages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "badpkg"),
        "cve_c directly depends on badpkg: {c}"
    );
    // A and B inherit exposure transitively from C.
    let a = by("cve_a").expect("cve_a exposed (inherited)");
    assert!(
        a["inherited_severity"].as_f64().unwrap() > 0.0,
        "cve_a inherits from C: {a}"
    );
    let b = by("cve_b").expect("cve_b exposed (inherited)");
    assert!(b["inherited_severity"].as_f64().unwrap() > 0.0, "{b}");
}
