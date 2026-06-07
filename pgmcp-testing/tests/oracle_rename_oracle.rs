mod common;

use common::{server_with_mock, text_of};
use pgmcp_testing::mocks::MockDbClient;

#[tokio::test]
async fn rename_oracle_normalizes_dedupes_and_reports_bounds() {
    let server = server_with_mock(MockDbClient::new());

    let res = server
        .call_tool_cli(
            "rename_oracle",
            serde_json::json!({
                "removed_name": " parse_config ",
                "current_names": ["render_page", "parse_configs", "parse_configs", "parseConfig"]
            }),
        )
        .await
        .expect("rename_oracle call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&res)).expect("json response");

    assert_eq!(v["removed_name"], "parse_config");
    assert_eq!(v["likely_rename_to"], "parse_configs");
    assert_eq!(v["candidate_count"], 3);
    assert_eq!(v["max_distance"], 2);
}

#[tokio::test]
async fn rename_oracle_rejects_invalid_inputs_before_building_dictionary() {
    let server = server_with_mock(MockDbClient::new());

    assert!(
        server
            .call_tool_cli(
                "rename_oracle",
                serde_json::json!({"removed_name": "   ", "current_names": ["parse_config"]}),
            )
            .await
            .is_err(),
        "blank removed_name must reject"
    );
    assert!(
        server
            .call_tool_cli(
                "rename_oracle",
                serde_json::json!({"removed_name": "parse_config", "current_names": ["   "]}),
            )
            .await
            .is_err(),
        "blank candidates must reject"
    );

    let overlong = "x".repeat(257);
    assert!(
        server
            .call_tool_cli(
                "rename_oracle",
                serde_json::json!({"removed_name": "parse_config", "current_names": [overlong]}),
            )
            .await
            .is_err(),
        "overlong candidates must reject before dictionary construction"
    );

    let too_many: Vec<String> = (0..5_001).map(|i| format!("candidate_{i}")).collect();
    assert!(
        server
            .call_tool_cli(
                "rename_oracle",
                serde_json::json!({"removed_name": "parse_config", "current_names": too_many}),
            )
            .await
            .is_err(),
        "candidate lists must be explicitly bounded"
    );
}
