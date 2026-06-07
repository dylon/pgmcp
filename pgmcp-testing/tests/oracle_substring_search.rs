mod common;

use common::{server_with_mock, text_of};
use pgmcp_testing::mocks::MockDbClient;

#[tokio::test]
async fn substring_search_preserves_exact_case_sensitive_semantics_and_dedupes() {
    let server = server_with_mock(MockDbClient::new());

    let res = server
        .call_tool_cli(
            "substring_search",
            serde_json::json!({
                "needle": "Beta",
                "haystack": ["alphaBeta", "alphaBeta", "alphabet"]
            }),
        )
        .await
        .expect("substring_search call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&res)).expect("json response");

    assert_eq!(v["needle"], "Beta");
    assert_eq!(v["haystack_size"], 2);
    assert_eq!(v["contains_substring"], true);

    let res = server
        .call_tool_cli(
            "substring_search",
            serde_json::json!({"needle": "beta", "haystack": ["alphaBeta"]}),
        )
        .await
        .expect("substring_search case-sensitive call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&res)).expect("json response");
    assert_eq!(v["contains_substring"], false);
}

#[tokio::test]
async fn substring_search_rejects_unbounded_inputs_before_index_build() {
    let server = server_with_mock(MockDbClient::new());

    assert!(
        server
            .call_tool_cli(
                "substring_search",
                serde_json::json!({"needle": "", "haystack": ["alpha"]}),
            )
            .await
            .is_err(),
        "empty needle must reject"
    );
    assert!(
        server
            .call_tool_cli(
                "substring_search",
                serde_json::json!({"needle": "a", "haystack": [""]}),
            )
            .await
            .is_err(),
        "empty haystack terms must reject"
    );

    let overlong = "x".repeat(4_097);
    assert!(
        server
            .call_tool_cli(
                "substring_search",
                serde_json::json!({"needle": overlong, "haystack": ["alpha"]}),
            )
            .await
            .is_err(),
        "overlong needle must reject"
    );

    let too_many: Vec<String> = (0..5_001).map(|i| format!("term_{i}")).collect();
    assert!(
        server
            .call_tool_cli(
                "substring_search",
                serde_json::json!({"needle": "term", "haystack": too_many}),
            )
            .await
            .is_err(),
        "haystack length must be explicitly bounded"
    );
}
