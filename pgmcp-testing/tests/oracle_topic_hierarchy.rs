//! Mocked-DB correctness oracle for `topic_hierarchy`.
//!
//! The tool runs agglomerative average-linkage clustering on the
//! provided centroid vectors. With three planted centroids near the
//! orthonormal basis vectors {e0, e1, e2}, every pair has cosine ≈ 0
//! (max distance) — but if we deliberately make two of them slightly
//! more similar than the third, the dendrogram's first merge must
//! pair those two.
//!
//! When `project` is omitted the tool reads centroids from the
//! `global` scope without invoking `run_project_topic_scan`, so a
//! pure mock is sufficient.

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::context::SystemContext;
use pgmcp::db::DbClient;
use pgmcp::db::queries::TopicCentroidRow;
use pgmcp::embed::EmbedSource;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::McpServer;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::fixtures::test_config;
use pgmcp_testing::mocks::{DeterministicEmbeddingBackend, MockDbClient};

const D: usize = 384;

fn server_with_mock(mock: MockDbClient) -> McpServer {
    let db: Arc<dyn DbClient> = Arc::new(mock);
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(test_config()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn pgmcp::embed::EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(384));
    let embed_source = EmbedSource::backend(embed_backend);
    let ctx =
        SystemContext::production(db, embed_source, stats, config, log_broadcaster, task_store);
    McpServer::new(ctx)
}

fn text_of(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .next()
        .expect("text content present")
}

fn unit(idx: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; D];
    v[idx] = 1.0;
    v
}

/// Centroid that is a 50/50 mix of two basis directions, L2-normalised.
fn mix(i: usize, j: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; D];
    v[i] = 1.0;
    v[j] = 1.0;
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    for x in &mut v {
        *x /= n;
    }
    v
}

#[tokio::test]
async fn topic_hierarchy_returns_requested_group_count_with_min_2() {
    // 4 centroids, num_groups=2 → tool clamps and returns 2 groups.
    let mut mock = MockDbClient::new();
    mock.topic_centroids = vec![
        TopicCentroidRow {
            topic_id: 1,
            label: "auth".into(),
            chunk_count: 10,
            centroid: unit(0),
        },
        TopicCentroidRow {
            topic_id: 2,
            label: "database".into(),
            chunk_count: 12,
            centroid: unit(1),
        },
        TopicCentroidRow {
            topic_id: 3,
            label: "logging".into(),
            chunk_count: 8,
            centroid: unit(2),
        },
        TopicCentroidRow {
            topic_id: 4,
            label: "metrics".into(),
            chunk_count: 6,
            centroid: unit(3),
        },
    ];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("topic_hierarchy", serde_json::json!({"num_groups": 2}))
        .await
        .expect("call");
    let payload = text_of(&result);
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    assert_eq!(v["num_groups"], 2);
    let groups = v["groups"].as_array().expect("groups array");
    assert_eq!(groups.len(), 2);
}

#[tokio::test]
async fn topic_hierarchy_first_merge_pairs_closest_two_centroids() {
    // Three centroids: A and B are the same direction (cosine 1.0);
    // C is orthogonal. The first merge in the dendrogram must pair A
    // and B (zero distance) before pulling in C.
    //
    // We choose num_groups=2 so the tool merges exactly once: A+B
    // become one group, C remains alone.
    let mut mock = MockDbClient::new();
    mock.topic_centroids = vec![
        TopicCentroidRow {
            topic_id: 10,
            label: "alpha".into(),
            chunk_count: 5,
            centroid: mix(0, 1),
        },
        TopicCentroidRow {
            topic_id: 11,
            label: "beta".into(),
            chunk_count: 5,
            // Same as alpha — should merge first.
            centroid: mix(0, 1),
        },
        TopicCentroidRow {
            topic_id: 12,
            label: "gamma".into(),
            chunk_count: 5,
            // Orthogonal to alpha/beta.
            centroid: unit(2),
        },
    ];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("topic_hierarchy", serde_json::json!({"num_groups": 2}))
        .await
        .expect("call");
    let payload = text_of(&result);
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    let groups = v["groups"].as_array().expect("groups");
    assert_eq!(groups.len(), 2, "should land at 2 groups");

    // Find the group with > 1 topic — that's the alpha+beta merge.
    let merged_group = groups
        .iter()
        .find(|g| {
            g["topics"]
                .as_array()
                .map(|t| t.len() >= 2)
                .unwrap_or(false)
        })
        .expect("a 2-topic merged group");
    let labels: std::collections::BTreeSet<&str> = merged_group["topics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["label"].as_str().unwrap())
        .collect();
    assert!(
        labels.contains("alpha") && labels.contains("beta"),
        "alpha and beta must be in the merged group, got {labels:?}"
    );
}

#[tokio::test]
async fn topic_hierarchy_emits_guidance_when_fewer_than_two_centroids() {
    let mut mock = MockDbClient::new();
    mock.topic_centroids = vec![TopicCentroidRow {
        topic_id: 1,
        label: "single".into(),
        chunk_count: 5,
        centroid: unit(0),
    }];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("topic_hierarchy", serde_json::json!({}))
        .await
        .expect("call");
    let payload = text_of(&result);
    assert!(
        payload.contains("Need at least 2 topics") || payload.contains("Run discover_topics"),
        "expected guidance for too-few centroids; got:\n{payload}"
    );
}
