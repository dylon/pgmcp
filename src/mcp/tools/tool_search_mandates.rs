//! `tool_search_mandates` — Phase 0 memory-server quick win.
//!
//! Adds a search surface for `durable_mandates`, which previously had a
//! single reader (`list_durable_mandates_for_project`, a project-scope
//! dump with no filtering or ranking). Phase 0 ships Postgres FTS over
//! `imperative || target`; the same tool gains a semantic mode after
//! Phase 1 cutover provisions a 1024d BGE-M3 embedding column.

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use tracing::{debug, error};

use crate::context::SystemContext;
use crate::mcp::server::SearchMandatesParams;

const VALID_POLARITIES: &[&str] = &[
    "always",
    "never",
    "prefer",
    "avoid",
    "remember",
    "from_now_on",
    "correction",
    "permission",
    "constraint",
    "mandate",
    "process_rule",
    "project_rule",
];

pub async fn tool_search_mandates(
    ctx: &SystemContext,
    params: SearchMandatesParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .memory_search_mandates
        .fetch_add(1, Ordering::Relaxed);

    let pool = ctx
        .db()
        .pool()
        .ok_or_else(|| McpError::internal_error("raw pool unavailable", None))?;

    let limit = params.limit.unwrap_or(20).clamp(1, 200);
    if params.query.trim().is_empty() {
        return Err(McpError::invalid_params("query must not be empty", None));
    }
    if let Some(p) = params.polarity.as_deref()
        && !VALID_POLARITIES.contains(&p)
    {
        return Err(McpError::invalid_params(
            format!(
                "invalid polarity '{}'; must be one of {:?}",
                p, VALID_POLARITIES
            ),
            None,
        ));
    }
    if let Some(s) = params.scope.as_deref()
        && !matches!(s, "project" | "workspace")
    {
        return Err(McpError::invalid_params(
            "scope must be 'project' or 'workspace'",
            None,
        ));
    }
    let mode = params
        .mode
        .as_deref()
        .unwrap_or("fts")
        .trim()
        .to_ascii_lowercase();
    if !matches!(mode.as_str(), "fts" | "semantic" | "hybrid") {
        return Err(McpError::invalid_params(
            "mode must be 'fts', 'semantic', or 'hybrid'",
            None,
        ));
    }

    debug!(
        tool = "search_mandates",
        query = %truncate(&params.query, 200),
        mode = %mode,
        polarity = params.polarity.as_deref().unwrap_or("*"),
        scope = params.scope.as_deref().unwrap_or("*"),
        project_id = params.project_id.unwrap_or(-1),
        limit,
        "MCP tool invoked",
    );

    // FTS is the default; semantic/hybrid embed the query and read the v31
    // `durable_mandates.embedding` column. Hybrid RRF-fuses both legs over a
    // widened candidate pool, then truncates to `limit`.
    let polarity = params.polarity.as_deref();
    let scope = params.scope.as_deref();
    let results = match mode.as_str() {
        "fts" => {
            crate::db::queries::search_mandates_fts(
                pool,
                &params.query,
                polarity,
                scope,
                params.project_id,
                limit,
            )
            .await
        }
        "semantic" => {
            let embedding = embed_mandate_query(ctx, &params.query).await?;
            let ef = ctx.config().load().vector.ef_search;
            crate::db::queries::search_mandates_semantic(
                pool,
                &embedding,
                polarity,
                scope,
                params.project_id,
                limit,
                ef,
            )
            .await
        }
        _ => {
            let embedding = embed_mandate_query(ctx, &params.query).await?;
            let ef = ctx.config().load().vector.ef_search;
            let pool_size = (limit.saturating_mul(4)).clamp(limit, 200);
            let fts = crate::db::queries::search_mandates_fts(
                pool,
                &params.query,
                polarity,
                scope,
                params.project_id,
                pool_size,
            )
            .await;
            let sem = crate::db::queries::search_mandates_semantic(
                pool,
                &embedding,
                polarity,
                scope,
                params.project_id,
                pool_size,
                ef,
            )
            .await;
            match (fts, sem) {
                (Ok(f), Ok(s)) => Ok(rrf_merge_mandates(f, s, limit)),
                (Err(e), _) | (_, Err(e)) => Err(e),
            }
        }
    }
    .map_err(|e| {
        error!(tool = "search_mandates", error = %e, "query failed");
        McpError::internal_error(format!("query failed: {}", e), None)
    })?;

    let count = results.len();
    // Shadow-ASR channel (Phase D2b): project-scoped effect distribution.
    let effect_breakdown = match ctx.db().pool() {
        Some(pool) => {
            crate::mcp::tools::sema_helpers::effects::effect_breakdown_json(pool, params.project_id)
                .await
        }
        None => serde_json::json!({}),
    };

    let json = serde_json::to_string_pretty(&serde_json::json!({
        "effect_breakdown": effect_breakdown,
        "count": count,
        "mode": mode,
        "results": results,
    }))
    .map_err(|e| McpError::internal_error(format!("serialization failed: {}", e), None))?;

    debug!(
        tool = "search_mandates",
        results = count,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![rmcp::model::Content::text(
        json,
    )]))
}

fn truncate(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    &s[..end]
}

/// Embed the mandate query for the semantic / hybrid legs. Maps an embed
/// failure to an MCP internal error (mirrors `recall_prompts`).
async fn embed_mandate_query(ctx: &SystemContext, query: &str) -> Result<Vec<f32>, McpError> {
    ctx.embed().embed_query(query).await.map_err(|e| {
        error!(tool = "search_mandates", error = %e, "embedding failed");
        McpError::internal_error(format!("embedding failed: {}", e), None)
    })
}

/// Reciprocal-rank fusion of the FTS and semantic mandate legs for `mode=hybrid`.
/// Each list contributes `1 / (60 + rank)` per shared mandate id; the fused score
/// is written into `rank`, and the merged set is truncated to `limit`.
fn rrf_merge_mandates(
    fts: Vec<crate::db::queries::MandateSearchResult>,
    sem: Vec<crate::db::queries::MandateSearchResult>,
    limit: i32,
) -> Vec<crate::db::queries::MandateSearchResult> {
    use std::collections::HashMap;
    const K: f64 = 60.0;
    let mut score: HashMap<i64, f64> = HashMap::new();
    let mut by_id: HashMap<i64, crate::db::queries::MandateSearchResult> = HashMap::new();
    for (rank, m) in fts.into_iter().enumerate() {
        *score.entry(m.id).or_insert(0.0) += 1.0 / (K + (rank as f64) + 1.0);
        by_id.entry(m.id).or_insert(m);
    }
    for (rank, m) in sem.into_iter().enumerate() {
        *score.entry(m.id).or_insert(0.0) += 1.0 / (K + (rank as f64) + 1.0);
        by_id.entry(m.id).or_insert(m);
    }
    let mut merged: Vec<crate::db::queries::MandateSearchResult> = by_id
        .into_values()
        .map(|mut m| {
            m.rank = Some(score.get(&m.id).copied().unwrap_or(0.0) as f32);
            m
        })
        .collect();
    merged.sort_by(|a, b| {
        b.rank
            .partial_cmp(&a.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    merged.truncate(limit.clamp(1, 200) as usize);
    merged
}
