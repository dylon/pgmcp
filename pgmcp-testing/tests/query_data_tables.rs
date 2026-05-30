//! Integration tests for the JSON data-table tool family (`data_table_*`).
//!
//! These execute real SQL against a `TestDatabase` (the v19 `data_tables` /
//! `data_table_columns` / `data_table_rows` schema), exercising every dispatched
//! `data_table_*` tool end-to-end through `McpServer::call_tool_cli` — which is
//! also what the Layer-D coverage gate (`query_inventory_vs_coverage.rs`)
//! requires for each dispatched tool. The test server wires a deterministic
//! 1024-d embedding backend, so embed-on-write (`create`/`alter`) and the
//! semantic `search` path run for real.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

fn body(result: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&text_of(result)).expect("tool body must be JSON")
}

/// Full lifecycle over a strict (typed-schema) table: create → describe → list
/// → insert → select(filter) → update(filter) → aggregate → report(text/csv) →
/// search → alter(add column) → delete(filter) → drop. Touches all 12 tools.
#[tokio::test]
async fn data_table_full_lifecycle() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    // create (typed columns ⇒ strict; `ok` carries a default).
    let created = server
        .call_tool_cli(
            "data_table_create",
            json!({
                "name": "bench_obs",
                "description": "nightly latency + throughput observations",
                "columns": [
                    {"name": "ts", "data_type": "timestamp", "required": true},
                    {"name": "metric", "data_type": "text", "required": true},
                    {"name": "value", "data_type": "number", "required": true},
                    {"name": "ok", "data_type": "boolean", "default": true}
                ]
            }),
        )
        .await
        .expect("data_table_create must not error");
    let created = body(&created);
    assert_eq!(created["table"]["name"], "bench_obs");
    assert_eq!(created["table"]["schema_mode"], "strict");
    assert_eq!(created["columns"].as_array().unwrap().len(), 4);

    // describe (no rows yet).
    let described = body(
        &server
            .call_tool_cli("data_table_describe", json!({"table": "bench_obs"}))
            .await
            .expect("data_table_describe must not error"),
    );
    assert_eq!(described["row_count"].as_i64(), Some(0));
    assert_eq!(described["columns"].as_array().unwrap().len(), 4);

    // list.
    let listed = body(
        &server
            .call_tool_cli("data_table_list", json!({}))
            .await
            .expect("data_table_list must not error"),
    );
    assert!(
        listed["tables"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "bench_obs"),
        "list must include bench_obs"
    );

    // insert (3 rows; `ok` defaulted on the rows that omit it).
    let inserted = body(
        &server
            .call_tool_cli(
                "data_table_insert",
                json!({
                    "table": "bench_obs",
                    "source": "verify.sh",
                    "rows": [
                        {"ts": "2026-05-20T14:03:00Z", "metric": "latency_ms", "value": 12.4},
                        {"ts": "2026-05-21T14:03:00Z", "metric": "latency_ms", "value": 11.8},
                        {"ts": "2026-05-22T14:05:00Z", "metric": "throughput", "value": 4200}
                    ]
                }),
            )
            .await
            .expect("data_table_insert must not error"),
    );
    assert_eq!(inserted["inserted"].as_i64(), Some(3));

    // select with an eq filter.
    let selected = body(
        &server
            .call_tool_cli(
                "data_table_select",
                json!({
                    "table": "bench_obs",
                    "filter": [{"field": "metric", "op": "eq", "value": "latency_ms"}],
                    "sort_by": "value",
                    "sort_dir": "asc"
                }),
            )
            .await
            .expect("data_table_select must not error"),
    );
    assert_eq!(selected["total"].as_i64(), Some(2));
    assert_eq!(selected["rows"].as_array().unwrap().len(), 2);

    // update rows matching a filter.
    let updated = body(
        &server
            .call_tool_cli(
                "data_table_update",
                json!({
                    "table": "bench_obs",
                    "filter": [{"field": "metric", "op": "eq", "value": "throughput"}],
                    "patch": {"ok": false}
                }),
            )
            .await
            .expect("data_table_update must not error"),
    );
    assert_eq!(updated["updated"].as_i64(), Some(1));

    // aggregate: group by metric, count + avg + max of value.
    let agg = body(
        &server
            .call_tool_cli(
                "data_table_aggregate",
                json!({
                    "table": "bench_obs",
                    "group_by": ["metric"],
                    "aggregations": [
                        {"func": "count"},
                        {"field": "value", "func": "avg", "alias": "avg_value"},
                        {"field": "value", "func": "max"}
                    ]
                }),
            )
            .await
            .expect("data_table_aggregate must not error"),
    );
    assert_eq!(agg["total_rows"].as_i64(), Some(3));
    let groups = agg["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "two metric groups");
    let latency = groups
        .iter()
        .find(|g| g["group"]["metric"] == "latency_ms")
        .expect("latency_ms group present");
    assert_eq!(latency["metrics"]["count"].as_i64(), Some(2));
    // avg(12.4, 11.8) == 11.1 .. 12.4 → 12.1
    assert!((latency["metrics"]["avg_value"].as_f64().unwrap() - 12.1).abs() < 1e-6);

    // report — plain text (box-drawing) and CSV.
    let report_text = body(
        &server
            .call_tool_cli(
                "data_table_report",
                json!({
                    "table": "bench_obs",
                    "format": "text",
                    "summary": {
                        "group_by": ["metric"],
                        "aggregations": [{"field": "value", "func": "avg", "alias": "avg_value"}]
                    }
                }),
            )
            .await
            .expect("data_table_report (text) must not error"),
    );
    let rendered = report_text["rendered"].as_str().unwrap();
    assert!(rendered.contains("bench_obs"), "report names the table");
    assert!(rendered.contains("DETAIL"), "report has a detail section");

    let report_csv = body(
        &server
            .call_tool_cli(
                "data_table_report",
                json!({"table": "bench_obs", "format": "csv"}),
            )
            .await
            .expect("data_table_report (csv) must not error"),
    );
    assert_eq!(report_csv["format"], "csv");
    assert!(report_csv["rendered"].as_str().unwrap().contains("metric"));

    // search — the table was embedded on create (deterministic backend).
    let searched = body(
        &server
            .call_tool_cli(
                "data_table_search",
                json!({"query": "latency throughput benchmark", "limit": 5}),
            )
            .await
            .expect("data_table_search must not error"),
    );
    assert!(
        searched["results"].is_array(),
        "search returns a results array"
    );

    // alter — add a column (and re-embed on the metadata change).
    let altered = body(
        &server
            .call_tool_cli(
                "data_table_alter",
                json!({
                    "table": "bench_obs",
                    "add_columns": [{"name": "note", "data_type": "text"}]
                }),
            )
            .await
            .expect("data_table_alter must not error"),
    );
    assert!(
        altered["columns"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "note"),
        "alter added the note column"
    );

    // delete rows by filter.
    let deleted = body(
        &server
            .call_tool_cli(
                "data_table_delete",
                json!({
                    "table": "bench_obs",
                    "filter": [{"field": "metric", "op": "eq", "value": "throughput"}]
                }),
            )
            .await
            .expect("data_table_delete must not error"),
    );
    assert_eq!(deleted["deleted"].as_i64(), Some(1));

    // drop the table (2 rows remain ⇒ under the confirm threshold, no confirm needed).
    let dropped = body(
        &server
            .call_tool_cli("data_table_drop", json!({"table": "bench_obs"}))
            .await
            .expect("data_table_drop must not error"),
    );
    assert_eq!(dropped["dropped"], json!(true));
    assert_eq!(dropped["rows_deleted"].as_i64(), Some(2));
}

/// An open (schemaless) table accepts arbitrary JSON objects and reports them.
#[tokio::test]
async fn data_table_open_table_accepts_free_form() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    let created = body(
        &server
            .call_tool_cli("data_table_create", json!({"name": "notes"}))
            .await
            .expect("create open table"),
    );
    assert_eq!(created["table"]["schema_mode"], "open");

    server
        .call_tool_cli(
            "data_table_insert",
            json!({"table": "notes", "rows": [
                {"kind": "decision", "detail": "use JSONB", "weight": 3},
                {"freeform": [1, 2, 3], "nested": {"a": true}}
            ]}),
        )
        .await
        .expect("insert free-form rows");

    let selected = body(
        &server
            .call_tool_cli("data_table_select", json!({"table": "notes"}))
            .await
            .expect("select open rows"),
    );
    assert_eq!(selected["total"].as_i64(), Some(2));
}

/// A strict table rejects a row that violates a declared column type.
#[tokio::test]
async fn data_table_strict_validation_rejects_bad_row() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    server
        .call_tool_cli(
            "data_table_create",
            json!({
                "name": "metrics",
                "columns": [{"name": "n", "data_type": "integer", "required": true}]
            }),
        )
        .await
        .expect("create strict table");

    // `n` declared integer; a string must be rejected as invalid_params.
    let bad = server
        .call_tool_cli(
            "data_table_insert",
            json!({"table": "metrics", "rows": [{"n": "not-an-int"}]}),
        )
        .await;
    assert!(
        bad.is_err(),
        "strict insert of a wrong-typed field must error, got: {bad:?}"
    );

    // A valid row is accepted.
    let good = body(
        &server
            .call_tool_cli(
                "data_table_insert",
                json!({"table": "metrics", "rows": [{"n": 42}]}),
            )
            .await
            .expect("valid strict insert"),
    );
    assert_eq!(good["inserted"].as_i64(), Some(1));
}
