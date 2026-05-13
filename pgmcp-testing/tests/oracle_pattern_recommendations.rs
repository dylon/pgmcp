//! Real-DB oracle for `recommend_design_patterns` and `review_design_patterns`
//! after the principle/code_smell kinds were added.
//!
//! Verifies the additive output schema (new `recommended_principles`,
//! `code_smells_to_avoid`, `principles_to_consider` fields) and that
//! paradigm inference routes Erlang/Elixir/Pony to `actor_model`.

use pgmcp_testing::pool_tool_helpers::server_with_pool;
use pgmcp_testing::require_test_db;
use serde_json::Value;

fn extract_json(call_result: &rmcp::model::CallToolResult) -> Value {
    for content in &call_result.content {
        if let rmcp::model::RawContent::Text(text_content) = &content.raw {
            return serde_json::from_str::<Value>(&text_content.text)
                .expect("tool emitted invalid JSON");
        }
    }
    panic!("tool returned no Text content block");
}

#[tokio::test(flavor = "multi_thread")]
async fn recommend_design_patterns_returns_all_four_kinds() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    let result = server
        .call_tool_cli(
            "recommend_design_patterns",
            serde_json::json!({
                "task": "Decouple sender from receiver across a message bus",
                "language": "java",
                "limit": 6,
            }),
        )
        .await
        .expect("recommend_design_patterns call");
    let body = extract_json(&result);

    for field in [
        "recommended_patterns",
        "recommended_principles",
        "anti_patterns_to_avoid",
        "code_smells_to_avoid",
    ] {
        let arr = body
            .get(field)
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("expected `{field}` array in output: {body}"));
        assert!(
            !arr.is_empty(),
            "expected non-empty `{field}`; got [] in {body}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn review_design_patterns_returns_principles_and_smells() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    let result = server
        .call_tool_cli(
            "review_design_patterns",
            serde_json::json!({
                "design": "function with 8 boolean flag parameters and 5 nested if/else branches",
                "language": "java",
                "limit": 6,
            }),
        )
        .await
        .expect("review_design_patterns call");
    let body = extract_json(&result);

    for field in [
        "anti_pattern_risks",
        "code_smells_to_avoid",
        "pattern_alternatives",
        "principles_to_consider",
    ] {
        let arr = body
            .get(field)
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("expected `{field}` array in output: {body}"));
        assert!(
            !arr.is_empty(),
            "expected non-empty `{field}`; got [] in {body}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn paradigm_inference_routes_erlang_to_actor_model() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    let result = server
        .call_tool_cli(
            "recommend_design_patterns",
            serde_json::json!({
                "task": "Build a fault-tolerant supervised process tree",
                "language": "erlang",
                "limit": 8,
            }),
        )
        .await
        .expect("recommend_design_patterns(erlang) call");
    let body = extract_json(&result);

    let paradigms = body
        .get("paradigms")
        .and_then(Value::as_array)
        .expect("paradigms array present");
    let paradigm_slugs: Vec<&str> = paradigms.iter().filter_map(Value::as_str).collect();
    assert!(
        paradigm_slugs.contains(&"actor_model"),
        "Erlang task should infer actor_model paradigm; got {paradigm_slugs:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn paradigm_inference_routes_rxjs_to_reactive() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    let result = server
        .call_tool_cli(
            "recommend_design_patterns",
            serde_json::json!({
                "task": "Compose async UI event streams with backpressure",
                "language": "rxjs",
                "limit": 6,
            }),
        )
        .await
        .expect("recommend_design_patterns(rxjs) call");
    let body = extract_json(&result);
    let paradigms = body
        .get("paradigms")
        .and_then(Value::as_array)
        .expect("paradigms array present");
    let paradigm_slugs: Vec<&str> = paradigms.iter().filter_map(Value::as_str).collect();
    assert!(
        paradigm_slugs.contains(&"reactive_programming"),
        "RxJS task should infer reactive_programming paradigm; got {paradigm_slugs:?}"
    );
}
