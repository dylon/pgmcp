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
use tracing::{debug, error, warn};

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
    let guidance = catalog_guidance(pool, rows.is_empty()).await;

    json_result(&json!({
        "query": query,
        "domain": domain,
        "result_count": rows.len(),
        "results": rows,
        "guidance": guidance,
    }))
}

/// Guidance line for `tool_catalog`. Always explains `enable_tools` / `call_tool`;
/// on an empty result it additionally (a) cross-links `toolbox_search` — EXTERNAL
/// installed developer tools (Coq/Rocq, TLA+, Z3, perf, valgrind…) live in a
/// separate catalog, not here — and (b) reports the real embedding-backfill status
/// from [`mcp_tool_catalog::counts`] instead of unconditionally asserting a
/// transient "still backfilling".
async fn catalog_guidance(pool: &PgPool, empty: bool) -> String {
    let mut g = String::from(
        "These are this server's OWN MCP tools. You currently see only your default working set in \
         tools/list; to use a tool listed here that is not yet visible, call enable_tools({names:[...]}) \
         (or enable_tools({query:\"...\"}) / {domain:\"...\"}) — it then appears natively. As a direct \
         fallback you can also call_tool({name, args}) without enabling.",
    );
    if empty {
        g.push_str(
            " No own-tools matched. For EXTERNAL installed developer tools (e.g. Coq/Rocq, TLA+/TLC, \
             Z3, perf, valgrind) use toolbox_search / toolbox_recommend instead — those live in a \
             separate catalog.",
        );
        if let Ok((total, missing)) = mcp_tool_catalog::counts(pool).await
            && missing > 0
        {
            g.push_str(&format!(
                " (Semantic ranking is degraded: {missing}/{total} tools are not yet embedded; \
                 keyword matching is used until the warm-up embed pass completes.)"
            ));
        }
    }
    g
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
/// description edits since the last boot propagate (and deleted tools are pruned),
/// then synchronously embed any NULL-embedding rows so `tool_catalog` semantic
/// ranking works out of the box — without waiting on the embedding-migration cron,
/// which is disabled by default (`[cron] embedding_migration_interval_secs = 0`).
/// The embed is a near-no-op on a warm restart: `upsert_tool` preserves unchanged
/// vectors, so only new/edited rows are missing. An embed failure is non-fatal —
/// the seed already succeeded and the tokenized keyword fallback covers discovery
/// until the next pass.
pub async fn warm_mcp_tool_catalog(ctx: &SystemContext) -> Result<(), McpError> {
    let pool = raw_pool(ctx)?;
    seed_mcp_tool_catalog(pool).await?;
    match reembed_missing(ctx, pool).await {
        Ok(n) if n > 0 => debug!(
            embedded = n,
            "mcp_tool_catalog: embedded NULL rows on warm-up"
        ),
        Ok(_) => {}
        Err(e) => {
            warn!(error = ?e, "mcp_tool_catalog warm-up embed failed; keyword fallback active until next pass")
        }
    }
    Ok(())
}

/// Synchronously embed every NULL-embedding catalog row using the same embedder
/// the query path uses (`ctx.embed()`), so the in-process vector matches what the
/// embedding-migration cron would write. Per-row single-query embed — fine for the
/// ~330 compact rows, and self-throttling (only NULL rows are returned). Mirrors
/// [`crate::mcp::tools::tool_toolbox`]'s `reembed_missing`.
async fn reembed_missing(ctx: &SystemContext, pool: &PgPool) -> Result<u64, McpError> {
    let batch = mcp_tool_catalog::ids_missing_embeddings(pool, 10_000)
        .await
        .map_err(sql_error("tool_catalog_reembed"))?;
    let mut embedded = 0u64;
    for (id, text) in batch {
        let embedding = ctx.embed().embed_query(&text).await.map_err(|e| {
            error!(tool = "tool_catalog_reembed", error = %e, "Embedding failed");
            McpError::internal_error(format!("Embedding failed: {e}"), None)
        })?;
        mcp_tool_catalog::update_embedding(pool, id, &embedding)
            .await
            .map_err(sql_error("tool_catalog_reembed"))?;
        embedded += 1;
    }
    Ok(embedded)
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
