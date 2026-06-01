//! `tool_concurrency_bottlenecks` — concurrency choke-point ranking over the
//! `sync_ops` skeleton, weighted by `file_metrics.pagerank` (the centrality
//! proxy `io_hotpath` uses). Four metrics: lock contention, channel imbalance,
//! spawn fan-out, and async stalls (blocking-in-async). Complements
//! `io_hotpath` / `hot_path_audit` (which weight I/O, not locks/channels).

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::ConcurrencyBottlenecksParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_concurrency_bottlenecks(
    ctx: &SystemContext,
    params: ConcurrencyBottlenecksParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "concurrency_bottlenecks", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let top = params.top.unwrap_or(20).clamp(1, 200);

    let err =
        |e: sqlx::Error| McpError::internal_error(format!("bottleneck query failed: {e}"), None);

    // (1) Lock contention: a single lock acquired by many distinct symbols,
    // weighted by the hottest acquirer file (pagerank). Shared with the
    // concurrency-scan cron so the snapshot metric matches this ranking.
    let lock_contention: Vec<_> =
        crate::db::queries::lock_contention_ranking(pool, project_id, top)
            .await
            .map_err(err)?
            .into_iter()
            .map(|r| {
                json!({
                    "resource_key": r.resource_key, "resource_kind": r.resource_kind,
                    "distinct_acquirers": r.distinct_acquirers, "total_acquires": r.total_acquires,
                    "max_pagerank": r.max_pagerank,
                    "contention_score": r.contention_score(),
                })
            })
            .collect();

    // (2) Channel imbalance: send vs receive count skew per channel.
    let chan_rows: Vec<(String, i64, i64, f64)> = sqlx::query_as(
        "SELECT so.resource_key,
                COUNT(*) FILTER (WHERE so.op_kind IN ('send', 'send_persistent')) AS sends,
                COUNT(*) FILTER (WHERE so.op_kind IN ('recv', 'recv_persistent')) AS recvs,
                COALESCE(MAX(fm.pagerank), 0.0) AS max_pagerank
         FROM sync_ops so
         JOIN file_symbols fs ON fs.id = so.symbol_id
         JOIN indexed_files f ON f.id = fs.file_id
         LEFT JOIN file_metrics fm ON fm.file_id = f.id
         WHERE f.project_id = $1 AND so.resource_kind = 'channel' AND so.resource_key IS NOT NULL
         GROUP BY so.resource_key",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(err)?;
    let mut channel_imbalance: Vec<_> = chan_rows
        .into_iter()
        .filter(|(_, s, r, _)| s != r)
        .map(|(key, sends, recvs, pr)| {
            let total = (sends + recvs).max(1) as f64;
            let imbalance = ((sends - recvs).abs() as f64) / total;
            json!({
                "channel": key, "send_count": sends, "recv_count": recvs,
                "imbalance": imbalance, "max_pagerank": pr,
            })
        })
        .collect();
    channel_imbalance.sort_by(|a, b| {
        b["imbalance"]
            .as_f64()
            .partial_cmp(&a["imbalance"].as_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    channel_imbalance.truncate(top as usize);

    // (3) Spawn fan-out: symbols that spawn many tasks/threads.
    let spawn_rows: Vec<(i64, String, String, i64)> = sqlx::query_as(
        "SELECT so.symbol_id, fs.name, f.relative_path, COUNT(*) AS spawn_count
         FROM sync_ops so
         JOIN file_symbols fs ON fs.id = so.symbol_id
         JOIN indexed_files f ON f.id = fs.file_id
         WHERE f.project_id = $1 AND so.op_kind = 'spawn'
         GROUP BY so.symbol_id, fs.name, f.relative_path
         ORDER BY spawn_count DESC
         LIMIT $2",
    )
    .bind(project_id)
    .bind(top)
    .fetch_all(pool)
    .await
    .map_err(err)?;
    let spawn_fanout: Vec<_> = spawn_rows
        .into_iter()
        .map(|(id, name, path, n)| json!({"symbol_id": id, "name": name, "file": path, "spawn_ops": n}))
        .collect();

    // (4) Async stalls: symbols that both await and do blocking I/O — they stall
    // the executor on a hot path.
    let stall_rows: Vec<(i64, String, String, f64)> = sqlx::query_as(
        "SELECT fs.id, fs.name, f.relative_path, COALESCE(fm.pagerank, 0.0)
         FROM file_symbols fs
         JOIN indexed_files f ON f.id = fs.file_id
         LEFT JOIN file_metrics fm ON fm.file_id = f.id
         WHERE f.project_id = $1
           AND EXISTS (SELECT 1 FROM symbol_effects se
                       WHERE se.symbol_id = fs.id AND se.effect IN ('await_point', 'async'))
           AND EXISTS (SELECT 1 FROM symbol_effects se
                       WHERE se.symbol_id = fs.id AND se.effect = 'blocking_io')
         ORDER BY COALESCE(fm.pagerank, 0.0) DESC
         LIMIT $2",
    )
    .bind(project_id)
    .bind(top)
    .fetch_all(pool)
    .await
    .map_err(err)?;
    let async_stalls: Vec<_> = stall_rows
        .into_iter()
        .map(|(id, name, path, pr)| json!({"symbol_id": id, "name": name, "file": path, "pagerank": pr}))
        .collect();

    json_result(&json!({
        "lock_contention": lock_contention,
        "channel_imbalance": channel_imbalance,
        "spawn_fanout": spawn_fanout,
        "async_stalls": async_stalls,
        "guidance": "Concurrency choke points = structure (file pagerank) × concurrency load. \
            lock_contention: one lock acquired across many symbols (serialization point); \
            channel_imbalance: send/recv skew (backpressure / dropped messages); \
            spawn_fanout: task-spawn storms; async_stalls: symbols that await AND block on I/O \
            (executor stalls). Requires the graph-analysis cron to have filled file_metrics."
    }))
}
