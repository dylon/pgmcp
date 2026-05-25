//! Integration smoke tests for the graph-roadmap MCP tools (Phases 1.1/1.2/2.6
//! function-level + Tier-A graph tools, and the Phase 3–4 connectivity /
//! spectral / DSM / CK / graph-aware-retrieval tools).
//!
//! Mirrors `query_smoke_mcp_tools.rs` (Layer A): seed the synthetic corpus,
//! call each tool via `McpServer::call_tool_cli`, assert it returns `Ok` — the
//! point is to catch SQL/schema drift and the "dispatched-but-untested" gap the
//! `query_inventory_vs_coverage` safety net guards. These also satisfy that
//! net's requirement that every `call_tool_cli` arm has a corresponding test.

mod common;

use common::server_with_pool;
use pgmcp_testing::fixtures::synthetic_corpus::SyntheticCorpus;
use pgmcp_testing::require_test_db;
use serde_json::json;

/// Tier-A + file-scope graph tools must not error on a corpus that has no
/// materialized import/call graph — they return an empty, well-formed envelope.
#[tokio::test]
async fn graph_structure_tools_smoke() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);
    let p = json!({ "project": "proj-auth" });

    server
        .call_tool_cli("articulation_points", p.clone())
        .await
        .expect("articulation_points must not error on a sparse corpus");
    server
        .call_tool_cli("hits", p.clone())
        .await
        .expect("hits must not error");
    server
        .call_tool_cli("dominator_tree", p.clone())
        .await
        .expect("dominator_tree must not error");
    server
        .call_tool_cli("graph_connectivity", p.clone())
        .await
        .expect("graph_connectivity must not error");
    server
        .call_tool_cli("spectral_analysis", p.clone())
        .await
        .expect("spectral_analysis must not error");
    server
        .call_tool_cli("architecture_dsm", p.clone())
        .await
        .expect("architecture_dsm must not error");
}

/// Function-level analytics read the materialized `function_metrics` /
/// call-graph tables; empty ⇒ a well-formed empty result.
#[tokio::test]
async fn function_level_graph_tools_smoke() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);
    let p = json!({ "project": "proj-auth" });

    server
        .call_tool_cli("central_functions", p.clone())
        .await
        .expect("central_functions must not error");
    server
        .call_tool_cli("function_communities", p.clone())
        .await
        .expect("function_communities must not error");
    server
        .call_tool_cli("function_kcore", p.clone())
        .await
        .expect("function_kcore must not error");
    server
        .call_tool_cli("recursive_clusters", p.clone())
        .await
        .expect("recursive_clusters must not error");
    server
        .call_tool_cli("extended_centrality", p.clone())
        .await
        .expect("extended_centrality must not error");
}

/// CK metrics over the corpus's symbols (no OO classes ⇒ empty, still Ok).
#[tokio::test]
async fn ck_metrics_smoke() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);

    server
        .call_tool_cli("ck_metrics", json!({ "project": "proj-auth" }))
        .await
        .expect("ck_metrics must not error on a corpus with no OO classes");
}

/// Graph-aware retrieval tools embed the query (the test backbone is 384-d, so
/// code_raptor_search returns its graceful empty path); all must return Ok.
#[tokio::test]
async fn code_retrieval_tools_smoke() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = server_with_pool(pool);
    let p = json!({ "project": "proj-auth", "query": "authenticate user" });

    server
        .call_tool_cli("code_ppr_search", p.clone())
        .await
        .expect("code_ppr_search must not error");
    server
        .call_tool_cli("code_path_search", p.clone())
        .await
        .expect("code_path_search must not error");
    server
        .call_tool_cli("code_raptor_search", p.clone())
        .await
        .expect("code_raptor_search must not error");
}
