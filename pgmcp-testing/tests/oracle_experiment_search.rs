//! Focused oracle coverage for `experiment_search`.

use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::db::DbClient;
use pgmcp::embed::{EmbedSource, EmbeddingBackend};
use pgmcp::error::{PgmcpError, Result as PgmcpResult};
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::McpServer;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::pool_tool_helpers::seed_project;
use pgmcp_testing::require_test_db;
use serde_json::{Value, json};

fn text_of(result: &rmcp::model::CallToolResult) -> &str {
    for content in &result.content {
        if let rmcp::model::RawContent::Text(text) = &content.raw {
            return &text.text;
        }
    }
    panic!("tool returned no text content");
}

struct FailingEmbeddingBackend;

#[async_trait]
impl EmbeddingBackend for FailingEmbeddingBackend {
    fn name(&self) -> &'static str {
        "failing-experiment-search"
    }

    async fn embed_one(&self, _text: &str) -> PgmcpResult<Vec<f32>> {
        Err(PgmcpError::Embedding(
            "forced experiment_search fallback".into(),
        ))
    }
}

fn server_with_failing_embedder(pool: sqlx::PgPool) -> McpServer {
    let db: Arc<dyn DbClient> = Arc::new(pool);
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(Config::default()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn EmbeddingBackend> = Arc::new(FailingEmbeddingBackend);
    let lifecycle = pgmcp::daemon_state::DaemonLifecycle::new();
    lifecycle.transition(pgmcp::daemon_state::DaemonPhase::Ready);
    let ctx = SystemContext::production(
        db,
        EmbedSource::backend(embed_backend),
        stats,
        config,
        log_broadcaster,
        task_store,
        lifecycle,
    );
    McpServer::new(ctx)
}

async fn insert_experiment(
    pool: &sqlx::PgPool,
    slug: &str,
    title: &str,
    kind: &str,
    project_id: i32,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO experiments (slug, title, question, context, kind, project_id)
         VALUES ($1, $2, 'Does arena allocation reduce dispatch latency?',
                 'arena allocation dispatch latency fallback oracle',
                 $3, $4)
         RETURNING id",
    )
    .bind(slug)
    .bind(title)
    .bind(kind)
    .bind(project_id)
    .fetch_one(pool)
    .await
    .expect("insert experiment")
}

async fn insert_hypothesis(pool: &sqlx::PgPool, experiment_id: i64, verdict: &str, valid_to: bool) {
    sqlx::query(
        "INSERT INTO experiment_hypotheses
            (experiment_id, statement, primary_metric, acceptance_criterion,
             verdict, valid_to)
         VALUES ($1, 'arena allocation lowers latency', 'latency_ms', '{}'::jsonb,
                 $2,
                 CASE WHEN $3::bool THEN now() ELSE NULL END)",
    )
    .bind(experiment_id)
    .bind(verdict)
    .bind(valid_to)
    .execute(pool)
    .await
    .expect("insert hypothesis");
}

#[tokio::test(flavor = "multi_thread")]
async fn experiment_search_fts_preserves_filters_and_bounds() {
    let db = require_test_db!();
    let pool = db.pool();

    let project_a = seed_project(pool, "exp-search-a", "/ws/exp-search-a").await;
    let project_b = seed_project(pool, "exp-search-b", "/ws/exp-search-b").await;

    let accepted = insert_experiment(
        pool,
        "arena-accepted",
        "Arena allocation dispatch latency accepted",
        "optimization",
        project_a,
    )
    .await;
    insert_hypothesis(pool, accepted, "accepted", false).await;

    let rejected = insert_experiment(
        pool,
        "arena-rejected",
        "Arena allocation dispatch latency rejected",
        "optimization",
        project_a,
    )
    .await;
    insert_hypothesis(pool, rejected, "rejected", false).await;

    let stale_accepted = insert_experiment(
        pool,
        "arena-stale-accepted",
        "Arena allocation dispatch latency stale accepted",
        "optimization",
        project_a,
    )
    .await;
    insert_hypothesis(pool, stale_accepted, "accepted", true).await;
    insert_hypothesis(pool, stale_accepted, "rejected", false).await;

    let other_project = insert_experiment(
        pool,
        "arena-other-project",
        "Arena allocation dispatch latency other project",
        "optimization",
        project_b,
    )
    .await;
    insert_hypothesis(pool, other_project, "accepted", false).await;

    let server = server_with_failing_embedder(pool.clone());
    let result = server
        .call_tool_cli(
            "experiment_search",
            json!({
                "query": " arena allocation dispatch latency ",
                "project_id": project_a,
                "kind": " optimization ",
                "verdict": " accepted ",
                "limit": 50_000
            }),
        )
        .await
        .expect("experiment_search fallback");
    let v: Value = serde_json::from_str(text_of(&result)).expect("json");
    assert_eq!(
        v["query"].as_str(),
        Some("arena allocation dispatch latency")
    );
    assert_eq!(v["project_id"].as_i64(), Some(i64::from(project_a)));
    assert_eq!(v["kind"].as_str(), Some("optimization"));
    assert_eq!(v["verdict"].as_str(), Some("accepted"));
    assert_eq!(v["limit"].as_u64(), Some(100));
    assert_eq!(v["search_mode"].as_str(), Some("fts"));
    assert_eq!(v["count"].as_u64(), Some(1), "{v:#}");
    assert_eq!(v["results"][0]["slug"].as_str(), Some("arena-accepted"));

    let body = v.to_string();
    assert!(!body.contains("arena-rejected"), "{v:#}");
    assert!(!body.contains("arena-stale-accepted"), "{v:#}");
    assert!(!body.contains("arena-other-project"), "{v:#}");
}

#[tokio::test(flavor = "multi_thread")]
async fn experiment_search_rejects_invalid_filters() {
    let db = require_test_db!();
    let server = server_with_failing_embedder(db.pool().clone());

    assert!(
        server
            .call_tool_cli("experiment_search", json!({ "query": "   " }))
            .await
            .is_err(),
        "blank query must fail closed"
    );
    assert!(
        server
            .call_tool_cli(
                "experiment_search",
                json!({ "query": "latency", "kind": "sideways" }),
            )
            .await
            .is_err(),
        "unknown kind must fail closed"
    );
    assert!(
        server
            .call_tool_cli(
                "experiment_search",
                json!({ "query": "latency", "verdict": "maybe" }),
            )
            .await
            .is_err(),
        "unknown verdict must fail closed"
    );
}
