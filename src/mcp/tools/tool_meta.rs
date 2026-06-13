//! Tool bodies for the adaptive-tool-surface meta-tools.
//!
//! `tool_catalog` (semantic/keyword browse over the server's own tool catalog)
//! lives here as a normal `SystemContext`-only body so it is CLI-dispatchable and
//! covered by the dispatch-coverage gate. The stateful meta-tools — `enable_tools`
//! / `disable_tools` (mutate per-session overlay + emit `tools/list_changed`) and
//! `call_tool` (generic dispatch) — are implemented inline in
//! `server/handlers/meta.rs` because they need `&McpServer` (the peer, the
//! dispatch table). The catalog-resolution helpers they share live here.

use std::collections::HashSet;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use sqlx::PgPool;
use std::sync::atomic::Ordering;
use tracing::{debug, error};

use crate::context::SystemContext;
use crate::db::mcp_tool_catalog::{self, ToolCatalogSearchRow};
use crate::mcp::server::{EnableToolsParams, ToolCatalogParams};
use crate::mcp::tool_domains;
use crate::mcp::tools::sota_helpers::json_result;

/// Default number of tools returned by `tool_catalog`.
const DEFAULT_CATALOG_LIMIT: i64 = 12;
/// Default number of tools enabled by `enable_tools(query=…)`.
const DEFAULT_ENABLE_LIMIT: i64 = 5;

/// `tool_catalog` body: browse/search the server's own MCP tools. Returns name +
/// one-line description + domain (+ score for a semantic query) — NOT full input
/// schemas; those arrive natively once a tool is `enable_tools`-ed.
pub async fn tool_tool_catalog(
    ctx: &SystemContext,
    params: ToolCatalogParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    ensure_mcp_catalog_seeded_if_empty(pool).await?;

    let limit = params.limit.unwrap_or(DEFAULT_CATALOG_LIMIT).clamp(1, 100);
    let domain = normalize_opt(params.domain);
    let query = normalize_opt(params.query);
    debug!(
        tool = "tool_catalog",
        ?domain,
        has_query = query.is_some(),
        limit,
        "MCP tool invoked"
    );

    let rows = search_catalog(ctx, pool, query.as_deref(), limit, domain.clone()).await?;

    json_result(&json!({
        "query": query,
        "domain": domain,
        "result_count": rows.len(),
        "results": rows,
        "guidance": "These are this server's OWN MCP tools. You currently see only your default \
    working set in tools/list; to use a tool listed here that is not yet visible, call \
    `enable_tools({names:[...]})` (or `enable_tools({query:\"...\"})` / `{domain:\"...\"}`) — it then \
    appears natively. As a direct fallback you can also `call_tool({name, args})` without enabling. \
    Empty results may mean embeddings are still backfilling — keyword matching is used until then.",
    }))
}

/// Resolve a `tool_catalog` / `enable_tools(query=…)` search to ranked rows.
/// Prefers semantic ranking; falls back to keyword `ILIKE` when there is no
/// query, when embeddings have not backfilled yet, or when the embedder is
/// unavailable. Never errors on a transient embed failure — discovery degrades
/// rather than breaks.
pub async fn search_catalog(
    ctx: &SystemContext,
    pool: &PgPool,
    query: Option<&str>,
    limit: i64,
    domain: Option<String>,
) -> Result<Vec<ToolCatalogSearchRow>, McpError> {
    let q = query.unwrap_or("").trim();
    if q.is_empty() {
        return mcp_tool_catalog::keyword_search(pool, "", limit, domain)
            .await
            .map_err(sql_error("tool_catalog"));
    }
    match ctx.embed().embed_query(q).await {
        Ok(embedding) => {
            let ef_search = ctx.config().load().vector.ef_search;
            let rows = mcp_tool_catalog::semantic_search(
                pool,
                &embedding,
                limit,
                ef_search,
                domain.clone(),
            )
            .await
            .map_err(sql_error("tool_catalog"))?;
            if rows.is_empty() {
                // Embeddings not backfilled yet — fall back to keyword.
                mcp_tool_catalog::keyword_search(pool, q, limit, domain)
                    .await
                    .map_err(sql_error("tool_catalog"))
            } else {
                Ok(rows)
            }
        }
        Err(e) => {
            debug!(error = %e, "tool_catalog embed failed; keyword fallback");
            mcp_tool_catalog::keyword_search(pool, q, limit, domain)
                .await
                .map_err(sql_error("tool_catalog"))
        }
    }
}

/// Resolve `enable_tools` params (names ∪ domain ∪ query) to a concrete,
/// validated set of live tool names. Explicit `names` and `domain` must refer to
/// real tools/domains (fail closed); `query` contributes its top semantic matches.
pub async fn resolve_enable_targets(
    ctx: &SystemContext,
    params: &EnableToolsParams,
) -> Result<Vec<String>, McpError> {
    let pool = raw_pool(ctx)?;
    ensure_mcp_catalog_seeded_if_empty(pool).await?;

    let mut out: HashSet<String> = HashSet::new();

    for name in &params.names {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        if tool_domains::domain_of(name).is_some() {
            out.insert(name.to_string());
        } else {
            return Err(McpError::invalid_params(
                format!("unknown tool '{name}' — use tool_catalog to find valid names"),
                None,
            ));
        }
    }

    if let Some(domain) = normalize_opt(params.domain.clone()) {
        let found = tool_domains::tools_in_domain(&domain);
        if found.is_empty() {
            return Err(McpError::invalid_params(
                format!("unknown domain '{domain}' — see tool_catalog domains"),
                None,
            ));
        }
        out.extend(found);
    }

    if let Some(query) = normalize_opt(params.query.clone()) {
        let limit = params.limit.unwrap_or(DEFAULT_ENABLE_LIMIT).clamp(1, 50);
        let rows = search_catalog(ctx, pool, Some(&query), limit, None).await?;
        out.extend(rows.into_iter().map(|r| r.name));
    }

    Ok(out.into_iter().collect())
}

/// Seed the catalog from the live `tools/list` if the table is empty (lazy
/// first-call path). The daemon's [`warm_mcp_tool_catalog`] re-seeds on every
/// boot so description edits propagate; this only covers a cold table.
pub async fn ensure_mcp_catalog_seeded_if_empty(pool: &PgPool) -> Result<(), McpError> {
    let (total, _missing) = mcp_tool_catalog::counts(pool)
        .await
        .map_err(sql_error("tool_catalog"))?;
    if total == 0 {
        debug!("mcp_tool_catalog is empty; seeding from the live tool list");
        seed_mcp_tool_catalog(pool).await?;
    }
    Ok(())
}

/// Upsert every live MCP tool (name, domain, description, input schema) into
/// `mcp_tool_catalog`, then prune rows whose tool no longer exists. Idempotent;
/// only NULLs (re-embeds) rows whose embedded prose actually changed. Returns the
/// number of live tools.
pub async fn seed_mcp_tool_catalog(pool: &PgPool) -> Result<u64, McpError> {
    let catalog = crate::mcp::server::McpServer::static_tool_catalog();
    let mut names: Vec<String> = Vec::with_capacity(catalog.len());
    for tool in &catalog {
        let name = tool.name.as_ref();
        let domain = tool_domains::domain_of(name).unwrap_or("");
        let description = tool.description.as_deref().unwrap_or("");
        let input_schema = serde_json::Value::Object((*tool.input_schema).clone()).to_string();
        mcp_tool_catalog::upsert_tool(pool, name, domain, description, &input_schema)
            .await
            .map_err(sql_error("tool_catalog_seed"))?;
        names.push(name.to_string());
    }
    mcp_tool_catalog::prune_missing(pool, &names)
        .await
        .map_err(sql_error("tool_catalog_seed"))?;
    Ok(names.len() as u64)
}

/// Daemon-startup warm path: re-seed the catalog from the live tool list so
/// description edits since the last boot propagate (and deleted tools are pruned).
/// Embedding stays with the embedding-migration cron.
pub async fn warm_mcp_tool_catalog(ctx: &SystemContext) -> Result<(), McpError> {
    let pool = raw_pool(ctx)?;
    seed_mcp_tool_catalog(pool).await.map(|_| ())
}

fn normalize_opt(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn raw_pool(ctx: &SystemContext) -> Result<&PgPool, McpError> {
    ctx.db().pool().ok_or_else(|| {
        McpError::internal_error("meta tools require a real Postgres pool".to_string(), None)
    })
}

fn sql_error(tool: &'static str) -> impl Fn(sqlx::Error) -> McpError {
    move |e| {
        error!(tool, error = %e, "MCP tool failed");
        McpError::internal_error(format!("{} failed: {}", tool, e), None)
    }
}
