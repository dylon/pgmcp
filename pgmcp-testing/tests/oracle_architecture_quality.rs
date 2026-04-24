//! Real-Postgres correctness oracle for `architecture_quality`.
//! 10 dimensions, every grade in {A,B,C,D,F}, overall score = mean
//! of dimension scores.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::require_test_db;

#[tokio::test]
async fn architecture_quality_returns_ten_dimensions_each_with_letter_grade() {
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
        10,
        "architecture_quality must report exactly 10 dimensions"
    );
    for d in dims {
        let grade = d["grade"].as_str().expect("grade");
        assert!(
            ["A", "B", "C", "D", "F"].contains(&grade),
            "unexpected grade '{grade}' on dimension {}",
            d["dimension"]
        );
    }
    let scores: Vec<f64> = dims
        .iter()
        .map(|d| d["score"].as_str().unwrap().parse::<f64>().unwrap_or(0.0))
        .collect();
    let mean = scores.iter().sum::<f64>() / 10.0;
    let overall: f64 = v["overall_score"].as_str().unwrap().parse().expect("parse");
    assert!(
        (mean - overall).abs() < 0.2,
        "overall_score {overall} should equal mean of dimensions {mean}"
    );
}
