//! Real-Postgres correctness oracle for `anomaly_detection`. The
//! synthetic graph corpus seeds 3 files at basis-0 (a, b, c), 1 at
//! basis-1 (util), 1 at basis-2 (api). The corpus centroid lies
//! near basis-0 (3 of 5 files), so util and api are the
//! basis-distant outliers and one of them must top the ranking.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::require_test_db;

#[tokio::test]
async fn anomaly_detection_surfaces_outlier_file_at_top_of_ranking() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "anomaly_detection",
            serde_json::json!({"project": "graph-proj"}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let anomalies = v["anomalies"].as_array().expect("anomalies");
    assert!(!anomalies.is_empty(), "expected at least one anomaly");
    let top = &anomalies[0];
    let top_path = top["path"].as_str().expect("path");
    assert!(
        top_path == "util/util.rs" || top_path == "api/api.rs",
        "top anomaly should be util or api (basis-1/basis-2 outliers); got {top_path}"
    );
}
