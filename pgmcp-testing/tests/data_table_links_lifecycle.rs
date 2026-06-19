//! Integration test for the data-table ⇄ work-item/experiment link tools
//! (ADR-023, v44): `data_table_link` / `data_table_unlink`, plus the link
//! surfacing in `data_table_describe`. Drives the dispatched tools through
//! `call_tool_cli` (the Layer-D coverage gate requirement) against a real DB.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

fn body(result: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&text_of(result)).expect("tool body must be JSON")
}

#[tokio::test]
async fn data_table_link_lifecycle() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    // A work-item to link to.
    let wid: i64 = sqlx::query_scalar(
        "INSERT INTO work_items (public_id, kind, status, title)
         VALUES ('dtlink-target-zzz', 'task', 'pending', 'link target')
         ON CONFLICT (public_id) DO UPDATE SET title = 'link target' RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("seed work item");

    // A data table.
    server
        .call_tool_cli(
            "data_table_create",
            json!({"name": "dtlink_bench_zzz", "description": "links test"}),
        )
        .await
        .expect("data_table_create");

    // Link it to the work-item.
    let linked = body(
        &server
            .call_tool_cli(
                "data_table_link",
                json!({
                    "table": "dtlink_bench_zzz",
                    "target_type": "work_item",
                    "target_id": wid,
                    "role": "measurements"
                }),
            )
            .await
            .expect("data_table_link"),
    );
    assert_eq!(linked["target_type"], "work_item");
    assert_eq!(linked["target_id"].as_i64(), Some(wid));

    // A bad target id must fail closed (no dangling links).
    assert!(
        server
            .call_tool_cli(
                "data_table_link",
                json!({"table": "dtlink_bench_zzz", "target_type": "work_item", "target_id": 999_999_999})
            )
            .await
            .is_err(),
        "linking to a nonexistent target must error"
    );

    // describe surfaces the link.
    let described = body(
        &server
            .call_tool_cli("data_table_describe", json!({"table": "dtlink_bench_zzz"}))
            .await
            .expect("data_table_describe"),
    );
    let links = described["links"].as_array().expect("links array");
    assert!(
        links.iter().any(|l| l["target_id"].as_i64() == Some(wid)
            && l["target_type"] == "work_item"
            && l["role"] == "measurements"),
        "describe must surface the link; got {links:?}"
    );

    // unlink.
    let unlinked = body(
        &server
            .call_tool_cli(
                "data_table_unlink",
                json!({"table": "dtlink_bench_zzz", "target_type": "work_item", "target_id": wid}),
            )
            .await
            .expect("data_table_unlink"),
    );
    assert_eq!(unlinked["removed"], true);
}
