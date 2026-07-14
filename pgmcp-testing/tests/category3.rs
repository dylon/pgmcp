//! Coverage + smoke test for the remaining functor tools (ADR-028, item 4):
//! effect_functor / naturality_gap / colimit_view. Drives each through
//! call_tool_cli (Layer-D coverage gate) and asserts the dispatch + shape.

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

fn body(r: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&text_of(r)).expect("tool body must be JSON")
}

#[tokio::test]
async fn functor_tools_dispatch_and_shape() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    sqlx::query(
        "INSERT INTO projects (workspace_path, path, name) VALUES ('/ws/cat3','/ws/cat3/p','cat3proj')
         ON CONFLICT (path) DO UPDATE SET name='cat3proj'",
    )
    .execute(&pool)
    .await
    .expect("project");

    // effect_functor — empty effects ok; monoid_generators array present.
    let ef = body(
        &server
            .call_tool_cli("effect_functor", json!({"project": "cat3proj"}))
            .await
            .expect("effect_functor"),
    );
    assert!(ef["monoid_generators"].is_array(), "{ef}");

    // naturality_gap — empty (no import edges / embeddings) but shape present.
    let ng = body(
        &server
            .call_tool_cli("naturality_gap", json!({"project": "cat3proj"}))
            .await
            .expect("naturality_gap"),
    );
    assert!(ng["gaps"].is_array(), "{ng}");

    // colimit_view — reads the unified matview; objects/components present.
    let cv = body(
        &server
            .call_tool_cli("colimit_view", json!({}))
            .await
            .expect("colimit_view"),
    );
    assert!(cv["diagram_components"].is_array(), "{cv}");
    assert!(cv["objects"].is_array(), "{cv}");
}
