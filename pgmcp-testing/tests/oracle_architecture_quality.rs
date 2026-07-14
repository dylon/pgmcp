//! Real-Postgres correctness oracle for `architecture_quality`.
//! 11 dimensions, every scorable grade in {A,B,C,D,F}, N/A dimensions excluded
//! from the overall score, and project-id scoped metric rows.

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::require_test_db;
use serde_json::Value;
use sqlx::PgPool;

async fn insert_project(pool: &PgPool, name: &str, workspace: &str) -> i32 {
    sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(workspace)
    .bind(format!("{workspace}/{name}"))
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("insert project")
}

async fn insert_file(pool: &PgPool, project_id: i32, path: &str, relative_path: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO indexed_files
         (project_id, path, relative_path, language, size_bytes, line_count, modified_at)
         VALUES ($1, $2, $3, 'rust', 10, 10, NOW())
         RETURNING id",
    )
    .bind(project_id)
    .bind(path)
    .bind(relative_path)
    .fetch_one(pool)
    .await
    .expect("insert file")
}

async fn insert_metric(
    pool: &PgPool,
    metric_project_id: i32,
    file_id: i64,
    coupling: i32,
    instability: f64,
    churn_rate: f64,
    fix_commit_ratio: f64,
) {
    sqlx::query(
        "INSERT INTO file_metrics
         (file_id, project_id, pagerank, afferent_coupling, efferent_coupling,
          instability, churn_rate, fix_commit_ratio)
         VALUES ($1, $2, 1.0, $3, 0, $4, $5, $6)",
    )
    .bind(file_id)
    .bind(metric_project_id)
    .bind(coupling)
    .bind(instability)
    .bind(churn_rate)
    .bind(fix_commit_ratio)
    .execute(pool)
    .await
    .expect("insert metric");
}

fn dimension<'a>(payload: &'a Value, name: &str) -> &'a Value {
    payload["dimensions"]
        .as_array()
        .expect("dimensions")
        .iter()
        .find(|dimension| dimension["dimension"] == name)
        .unwrap_or_else(|| panic!("missing dimension {name} in payload: {payload}"))
}

fn score(payload: &Value, name: &str) -> f64 {
    dimension(payload, name)["score"]
        .as_str()
        .unwrap_or_else(|| panic!("missing score for {name}: {payload}"))
        .parse::<f64>()
        .unwrap_or_else(|err| panic!("non-numeric score for {name}: {err}; payload: {payload}"))
}

#[tokio::test]
async fn architecture_quality_returns_eleven_dimensions_each_with_letter_grade() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "architecture_quality",
            serde_json::json!({"project": "graph-proj"}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let dims = v["dimensions"].as_array().expect("dimensions");
    assert_eq!(
        dims.len(),
        11,
        "architecture_quality must report exactly 11 dimensions (incl. main_sequence_distance)"
    );
    let mut scores = Vec::new();
    for d in dims {
        let grade = d["grade"].as_str().expect("grade");
        assert!(
            ["A", "B", "C", "D", "F", "N/A"].contains(&grade),
            "unexpected grade '{grade}' on dimension {}",
            d["dimension"]
        );
        match d["score"].as_str().expect("score") {
            "N/A" => assert_eq!(grade, "N/A", "N/A score must have N/A grade"),
            score => scores.push(score.parse::<f64>().expect("numeric score")),
        }
    }
    assert!(
        !scores.is_empty(),
        "architecture_quality must have at least one scorable dimension"
    );
    let mean = scores.iter().sum::<f64>() / scores.len() as f64;
    let overall: f64 = v["overall_score"].as_str().unwrap().parse().expect("parse");
    assert!(
        (mean - overall).abs() < 0.2,
        "overall_score {overall} should equal mean of dimensions {mean}"
    );
}

#[tokio::test]
async fn architecture_quality_normalizes_project_and_validates_detail() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "architecture_quality",
            serde_json::json!({"project": " graph-proj ", "detail": " full "}),
        )
        .await
        .expect("trimmed full-detail call");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["project"], "graph-proj");
    assert_eq!(v["detail"], "full");
    assert!(
        v["dimensions"]
            .as_array()
            .expect("dimensions")
            .iter()
            .all(|dimension| dimension["description"].is_string()),
        "full detail must include dimension descriptions: {v}"
    );

    let err = server
        .call_tool_cli(
            "architecture_quality",
            serde_json::json!({"project": "graph-proj", "detail": "verbose"}),
        )
        .await
        .expect_err("invalid detail must fail closed");
    assert!(
        err.to_string().contains("Unknown detail"),
        "unexpected invalid-detail error: {err}"
    );
}

#[tokio::test]
async fn architecture_quality_rejects_duplicate_project_display_names() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind("/ws/arch-dup-a")
        .bind("/ws/arch-dup-a/arch-dup")
        .bind("arch-dup")
        .execute(&pool)
        .await
        .expect("insert first duplicate project");
    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind("/ws/arch-dup-b")
        .bind("/ws/arch-dup-b/arch-dup")
        .bind("arch-dup")
        .execute(&pool)
        .await
        .expect("insert second duplicate project");
    let server = server_with_pool(pool);

    let err = server
        .call_tool_cli(
            "architecture_quality",
            serde_json::json!({"project": "arch-dup"}),
        )
        .await
        .expect_err("duplicate project display names must fail closed");
    assert!(
        err.to_string().contains("ambiguous project name"),
        "unexpected duplicate-project error: {err}"
    );
}

#[tokio::test]
async fn architecture_quality_ignores_cross_project_metric_and_edge_rows() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = insert_project(&pool, "arch-scope", "/ws/arch-scope").await;
    let other_project_id = insert_project(&pool, "arch-other", "/ws/arch-other").await;
    let file_id = insert_file(&pool, project_id, "/ws/arch-scope/src/a.rs", "src/a.rs").await;
    let other_file_id = insert_file(
        &pool,
        other_project_id,
        "/ws/arch-other/src/b.rs",
        "src/b.rs",
    )
    .await;

    insert_metric(&pool, project_id, file_id, 0, 0.1, 0.0, 0.0).await;
    // Deliberately inconsistent row: its denormalized project_id points at the
    // requested project, but the file belongs to another project. It must not
    // affect averages or SDP violation detection.
    insert_metric(&pool, project_id, other_file_id, 40, 0.9, 5.0, 1.0).await;
    sqlx::query(
        "INSERT INTO code_graph_edges
         (project_id, source_file_id, target_file_id, edge_type, weight)
         VALUES ($1, $2, $3, 'import', 1.0)",
    )
    .bind(project_id)
    .bind(file_id)
    .bind(other_file_id)
    .execute(&pool)
    .await
    .expect("insert stale cross-project edge");

    let server = server_with_pool(pool);
    let result = server
        .call_tool_cli(
            "architecture_quality",
            serde_json::json!({"project": "arch-scope", "detail": "full"}),
        )
        .await
        .expect("architecture_quality call");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("json");

    assert_eq!(score(&v, "loose_coupling"), 100.0);
    assert_eq!(score(&v, "api_stability"), 100.0);
    assert_eq!(score(&v, "dependency_health"), 100.0);
    assert_eq!(score(&v, "sdp_compliance"), 100.0);
}

#[tokio::test]
async fn degenerate_coupling_marks_coupling_dims_na_and_excludes_them() {
    // A 25-file project where EVERY file has zero coupling (the import-edge
    // resolution failure) is degenerate: the coupling-derived dims must go N/A
    // rather than score a falsely-excellent ~100, and be excluded from overall.
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = insert_project(&pool, "arch-degen", "/ws/arch-degen").await;
    for i in 0..25 {
        let fid = insert_file(
            &pool,
            project_id,
            &format!("/ws/arch-degen/src/f{i}.rs"),
            &format!("src/f{i}.rs"),
        )
        .await;
        insert_metric(&pool, project_id, fid, 0, 0.0, 0.0, 0.0).await;
    }
    let server = server_with_pool(pool);
    let result = server
        .call_tool_cli(
            "architecture_quality",
            serde_json::json!({"project": "arch-degen", "detail": "full"}),
        )
        .await
        .expect("call");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("json");

    for dim in [
        "loose_coupling",
        "sdp_compliance",
        "code_organization",
        "main_sequence_distance",
    ] {
        assert_eq!(
            dimension(&v, dim)["score"].as_str(),
            Some("N/A"),
            "{dim} must be N/A under degenerate coupling: {v}"
        );
    }
    // overall_score is the mean of ONLY the scorable (non-N/A) dimensions.
    let scores: Vec<f64> = v["dimensions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["score"].as_str().and_then(|s| s.parse::<f64>().ok()))
        .collect();
    let mean = scores.iter().sum::<f64>() / scores.len() as f64;
    let overall: f64 = v["overall_score"].as_str().unwrap().parse().unwrap();
    assert!(
        (mean - overall).abs() < 0.2,
        "overall {overall} must exclude the N/A coupling dims (mean {mean})"
    );
}

#[tokio::test]
async fn main_sequence_distance_scored_from_rollup_when_types_present() {
    // Non-degenerate project (few files, real coupling) with persisted type
    // symbols and a project_metrics rollup ⇒ the main-sequence-distance dimension
    // is scored as 100·(1 − avg_distance).
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = insert_project(&pool, "arch-msd", "/ws/arch-msd").await;
    let fid = insert_file(&pool, project_id, "/ws/arch-msd/src/a.rs", "src/a.rs").await;
    sqlx::query(
        "INSERT INTO file_metrics
         (file_id, project_id, pagerank, afferent_coupling, efferent_coupling, instability,
          abstract_type_count, concrete_type_count)
         VALUES ($1, $2, 1.0, 1, 1, 0.5, 1, 2)",
    )
    .bind(fid)
    .bind(project_id)
    .execute(&pool)
    .await
    .expect("insert metric with type counts");
    // avg_distance = 0.25 (exact in f64, comfortably mid-grade) ⇒ score 75.0,
    // grade C — chosen off any grade boundary so float formatting can't flip it.
    sqlx::query(
        "INSERT INTO project_metrics
         (project_id, file_count, module_count, avg_instability, avg_abstractness, avg_distance,
          architecture_quality_score)
         VALUES ($1, 1, 1, 0.5, 0.33, 0.25, 0.75)",
    )
    .bind(project_id)
    .execute(&pool)
    .await
    .expect("insert project_metrics rollup");

    let server = server_with_pool(pool);
    let result = server
        .call_tool_cli(
            "architecture_quality",
            serde_json::json!({"project": "arch-msd", "detail": "full"}),
        )
        .await
        .expect("call");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("json");
    // (1 − 0.25)·100 = 75 ⇒ grade C.
    assert!(
        (score(&v, "main_sequence_distance") - 75.0).abs() < 0.5,
        "expected ~75 main-sequence-distance score: {v}"
    );
    assert_eq!(
        dimension(&v, "main_sequence_distance")["grade"].as_str(),
        Some("C")
    );
}
