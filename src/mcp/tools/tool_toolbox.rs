//! MCP tools for the developer-tool ("toolbox") catalog: the formal-verification
//! tools and profiling/benchmarking/debugging tools installed on this machine.
//!
//! Single-table catalog (`tool_cards`, migration v32) seeded from
//! `src/tools_catalog/`. Embeddings are backfilled by the embedding-migration
//! cron (the 1024d-direct pattern); the seed/warm path does NOT embed inline.
//! `toolbox_refresh{mode:reembed}` force-embeds NULL rows for immediate use.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use sqlx::PgPool;
use tracing::{debug, error};

use crate::context::SystemContext;
use crate::db::tool_cards::{self, ToolListOptions, ToolSearchOptions};
use crate::mcp::server::*;
use crate::tools_catalog;

const DEFAULT_SEARCH_LIMIT: i32 = 10;
const DEFAULT_LIST_LIMIT: i32 = 50;
const DEFAULT_RECOMMEND_LIMIT: i32 = 8;

pub async fn tool_toolbox_search(
    ctx: &SystemContext,
    params: ToolboxSearchParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    ensure_toolbox_seeded_if_empty(pool).await?;

    let limit = params.limit.unwrap_or(DEFAULT_SEARCH_LIMIT).clamp(1, 50);
    debug!(
        tool = "toolbox_search",
        query_len = params.query.len(),
        limit,
        "MCP tool invoked"
    );

    let rows = search_tools(
        ctx,
        pool,
        &params.query,
        limit,
        ToolSearchOptions {
            domain: params.domain,
            category: params.category,
        },
    )
    .await?;

    json_result(json!({
        "query": params.query,
        "result_count": rows.len(),
        "results": rows,
        "guidance": "Tool cards from the local toolbox catalog (installed formal-verification + \
    profiling/benchmarking/debugging tools). 'invocation' is grounded on this machine; consult a \
    card's 'alternatives' (via toolbox_get) for tradeoffs. Empty results may mean embeddings are \
    still backfilling — run toolbox_refresh{mode:reembed} or wait for the embedding cron.",
    }))
}

pub async fn tool_toolbox_get(
    ctx: &SystemContext,
    params: ToolboxGetParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    ensure_toolbox_seeded_if_empty(pool).await?;

    let Some(tool) = tool_cards::get_tool_card(pool, &params.slug_or_id)
        .await
        .map_err(sql_error("toolbox_get"))?
    else {
        return json_result(json!({
            "found": false,
            "slug_or_id": params.slug_or_id,
            "message": "No tool card found for slug_or_id",
        }));
    };

    json_result(json!({ "found": true, "tool": tool }))
}

pub async fn tool_toolbox_list(
    ctx: &SystemContext,
    params: ToolboxListParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    ensure_toolbox_seeded_if_empty(pool).await?;

    let limit = params.limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, 200);
    let offset = params.offset.unwrap_or(0).max(0);
    let rows = tool_cards::list_tool_cards(
        pool,
        ToolListOptions {
            domain: params.domain,
            category: params.category,
            limit,
            offset,
        },
    )
    .await
    .map_err(sql_error("toolbox_list"))?;

    json_result(json!({
        "count": rows.len(),
        "limit": limit,
        "offset": offset,
        "tools": rows,
    }))
}

pub async fn tool_toolbox_recommend(
    ctx: &SystemContext,
    params: ToolboxRecommendParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    ensure_toolbox_seeded_if_empty(pool).await?;

    let limit = params.limit.unwrap_or(DEFAULT_RECOMMEND_LIMIT).clamp(1, 30);
    let domain = params.domain.clone().or_else(|| infer_domain(&params.task));
    let query = build_recommend_query(&params.task, params.constraints.as_deref());

    let rows = search_tools(
        ctx,
        pool,
        &query,
        limit,
        ToolSearchOptions {
            domain: domain.clone(),
            category: None,
        },
    )
    .await?;

    json_result(json!({
        "task": params.task,
        "domain": domain,
        "result_count": rows.len(),
        "recommended_tools": rows,
        "planning_guidance": "Ranked installed tools for the task. Read each card's 'when_to_use' \
    and 'invocation', then consult 'alternatives' for tradeoffs. If empty, embeddings may still be \
    backfilling — run toolbox_refresh{mode:reembed}.",
    }))
}

pub async fn tool_toolbox_stats(ctx: &SystemContext) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    ensure_toolbox_seeded_if_empty(pool).await?;
    let stats = tool_cards::catalog_stats(pool)
        .await
        .map_err(sql_error("toolbox_stats"))?;

    json_result(json!({ "stats": stats }))
}

pub async fn tool_toolbox_refresh(
    ctx: &SystemContext,
    params: ToolboxRefreshParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    let mode = params.mode.unwrap_or_else(|| "seed_only".to_string());
    let dry_run = params.dry_run.unwrap_or(false);

    let categories = tools_catalog::tool_category_seeds();
    let tools = tools_catalog::tool_seeds();

    if dry_run {
        return json_result(json!({
            "dry_run": true,
            "mode": mode,
            "categories_seen": categories.len(),
            "tools_seen": tools.len(),
        }));
    }

    for category in &categories {
        tool_cards::upsert_tool_category(pool, category)
            .await
            .map_err(sql_error("toolbox_refresh"))?;
    }
    let mut tools_upserted = 0u64;
    for seed in &tools {
        tool_cards::upsert_tool_card(pool, seed)
            .await
            .map_err(sql_error("toolbox_refresh"))?;
        tools_upserted += 1;
    }

    let embedded = if mode == "reembed" {
        reembed_missing(ctx, pool).await?
    } else {
        0
    };

    json_result(json!({
        "mode": mode,
        "dry_run": false,
        "summary": {
            "categories_upserted": categories.len(),
            "tools_upserted": tools_upserted,
            "embedded": embedded,
        },
        "note": if mode == "reembed" {
            "Re-upserted cards and synchronously embedded NULL-embedding rows."
        } else {
            "Re-upserted cards; the embedding-migration cron will (re)embed NULL rows. Pass \
    mode=reembed to embed in-process now."
        },
    }))
}

/// Background seed entry-point invoked from the daemon at startup so the first
/// MCP toolbox-tool call doesn't seed lazily. Embedding stays with the cron.
pub async fn warm_toolbox_catalog(ctx: &SystemContext) -> Result<(), McpError> {
    let pool = raw_pool(ctx)?;
    ensure_toolbox_seeded_if_empty(pool).await
}

async fn search_tools(
    ctx: &SystemContext,
    pool: &PgPool,
    query: &str,
    limit: i32,
    options: ToolSearchOptions,
) -> Result<Vec<tool_cards::ToolCardSearchRow>, McpError> {
    let embedding = ctx.embed().embed_query(query).await.map_err(|e| {
        error!(tool = "toolbox", error = %e, "Embedding failed");
        McpError::internal_error(format!("Embedding failed: {}", e), None)
    })?;
    let ef_search = ctx.config().load().vector.ef_search;
    tool_cards::semantic_search_tool_cards(pool, &embedding, limit, ef_search, options)
        .await
        .map_err(sql_error("toolbox_search"))
}

async fn ensure_toolbox_seeded_if_empty(pool: &PgPool) -> Result<(), McpError> {
    let count = tool_cards::count_tool_cards(pool)
        .await
        .map_err(sql_error("toolbox"))?;
    if count == 0 {
        debug!("Toolbox catalog is empty; seeding bundled tool cards");
        seed_toolbox_catalog(pool).await?;
    }
    Ok(())
}

/// Upsert the bundled categories + tool cards. Does NOT embed — the
/// embedding-migration cron backfills the `embedding` column (the established
/// 1024d-direct pattern; avoids GPU contention on the boot/seed path).
async fn seed_toolbox_catalog(pool: &PgPool) -> Result<(), McpError> {
    for category in tools_catalog::tool_category_seeds() {
        tool_cards::upsert_tool_category(pool, &category)
            .await
            .map_err(sql_error("toolbox_seed"))?;
    }
    for seed in tools_catalog::tool_seeds() {
        tool_cards::upsert_tool_card(pool, &seed)
            .await
            .map_err(sql_error("toolbox_seed"))?;
    }
    Ok(())
}

/// Synchronously embed every NULL-embedding card (the `toolbox_refresh`
/// reembed path). Uses the single-query embedder per row — fine for an
/// occasional admin action over ~100 compact cards.
async fn reembed_missing(ctx: &SystemContext, pool: &PgPool) -> Result<u64, McpError> {
    let batch = tool_cards::ids_missing_embeddings(pool, 10_000)
        .await
        .map_err(sql_error("toolbox_refresh"))?;
    let mut embedded = 0u64;
    for (id, text) in batch {
        let embedding = ctx.embed().embed_query(&text).await.map_err(|e| {
            error!(tool = "toolbox_refresh", error = %e, "Embedding failed");
            McpError::internal_error(format!("Embedding failed: {}", e), None)
        })?;
        tool_cards::update_tool_card_embedding(pool, id, &embedding)
            .await
            .map_err(sql_error("toolbox_refresh"))?;
        embedded += 1;
    }
    Ok(embedded)
}

/// Heuristic domain inference from the task text. Returns `None` (search both
/// domains) when the signal is absent or ambiguous.
fn infer_domain(task: &str) -> Option<String> {
    let t = task.to_lowercase();
    const FV_KW: &[&str] = &[
        "prove",
        "proof",
        "verify",
        "verif",
        "terminat",
        "theorem",
        "invariant",
        "model check",
        "model-check",
        "smt",
        "sat solver",
        "confluence",
        "refinement",
        "separation logic",
        "protocol",
        "race-free",
        "data-race free",
        "data race free",
        "gröbner",
        "grobner",
        "semidefinite",
        "sound",
        "decision procedure",
    ];
    const DEV_KW: &[&str] = &[
        "profile",
        "profiling",
        "benchmark",
        "trace",
        "tracing",
        "debug",
        "debugger",
        "flame",
        "heap",
        "memory leak",
        "leak",
        "cpu",
        "perf ",
        "off-cpu",
        "syscall",
        "race condition",
        "sanitiz",
        "latency",
        "bottleneck",
        "frequency",
        "governor",
        "allocation",
        "hotspot",
        "monitor",
        "bandwidth",
    ];
    let fv = FV_KW.iter().any(|k| t.contains(k));
    let dev = DEV_KW.iter().any(|k| t.contains(k));
    match (fv, dev) {
        (true, false) => Some("formal_verification".to_string()),
        (false, true) => Some("developer_tooling".to_string()),
        _ => None,
    }
}

fn build_recommend_query(task: &str, constraints: Option<&[String]>) -> String {
    match constraints {
        Some(cs) if !cs.is_empty() => format!("{task}. Constraints: {}", cs.join("; ")),
        _ => task.to_string(),
    }
}

fn raw_pool(ctx: &SystemContext) -> Result<&PgPool, McpError> {
    ctx.db().pool().ok_or_else(|| {
        McpError::internal_error(
            "toolbox tools require a real Postgres pool".to_string(),
            None,
        )
    })
}

fn sql_error(tool: &'static str) -> impl Fn(sqlx::Error) -> McpError {
    move |e| {
        error!(tool, error = %e, "MCP tool failed");
        McpError::internal_error(format!("{} failed: {}", tool, e), None)
    }
}

fn json_result(value: serde_json::Value) -> Result<CallToolResult, McpError> {
    let json = serde_json::to_string_pretty(&value)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}
