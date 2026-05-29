//! `tool_hybrid_search` — MCP tool body, extracted from `super::super::server`.

#![allow(unused_imports)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, LoggingLevel};
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::context::SystemContext;
use crate::mcp::server::*;

/// Reciprocal Rank Fusion constant. Standard literature value (Cormack
/// et al., "Reciprocal Rank Fusion outperforms Condorcet and individual
/// Rank Learning Methods", SIGIR 2009). Higher k flattens the score
/// curve so lower-ranked results contribute relatively more; lower k
/// emphasizes the top of each list.
pub const RRF_K: f64 = 60.0;

/// Per-result RRF contribution from a ranked list.
///
///   `score = weight / (k + rank + 1)`
///
/// `rank` is 0-indexed (top result has rank 0). `weight` lets callers
/// blend two sources with different importance (e.g. BM25 vs vector).
///
/// Extracted as a pub helper so oracle tests can pin the formula
/// without spinning up the full hybrid_search tool.
#[inline]
pub fn rrf_score(weight: f64, k: f64, rank: usize) -> f64 {
    weight / (k + rank as f64 + 1.0)
}

/// Per-leg outcome, reported in the `leg_status` response field so a caller
/// can see when `hybrid_search` degraded. A single leg's `Error`/`Timeout`
/// no longer fails the whole tool — it contributes nothing and the surviving
/// legs' results are still fused and returned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LegStatus {
    /// The leg ran and contributed its ranked list.
    Ok,
    /// The leg was not run — its weight was 0, or an optional precondition
    /// (e.g. a per-project HybridLM model) was absent.
    Skipped,
    /// The leg returned an error (e.g. a database error) and contributed nothing.
    Error,
    /// The leg exceeded its time budget and contributed nothing.
    Timeout,
}

impl LegStatus {
    fn label(self) -> &'static str {
        match self {
            LegStatus::Ok => "ok",
            LegStatus::Skipped => "skipped",
            LegStatus::Error => "error",
            LegStatus::Timeout => "timeout",
        }
    }
}

pub async fn tool_hybrid_search(
    ctx: &SystemContext,
    params: HybridSearchParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().hybrid_searches.fetch_add(1, Ordering::Relaxed);

    // Cold-start fast-fail: surface a clear, retryable signal rather than
    // parking the request in the bounded query channel until a worker finishes
    // loading its model. Only fires during the brief warmup window.
    if !ctx.embed().is_ready() {
        return Err(McpError::internal_error(
            "embedder is still warming up (loading model); retry shortly",
            None,
        ));
    }

    let limit = params.limit.unwrap_or(20);
    let bm25_weight = params.bm25_weight.unwrap_or(0.5);
    let semantic_weight = params.semantic_weight.unwrap_or(0.5);
    let wfst_lm_weight = params.wfst_lm_weight.unwrap_or(1.0);
    let max_query_edit_distance = params.max_query_edit_distance.unwrap_or(2);

    debug!(
        tool = "hybrid_search",
        query = %truncate(&params.query, 200),
        project = params.project.as_deref().unwrap_or("*"),
        language = params.language.as_deref().unwrap_or("*"),
        limit,
        bm25_weight,
        semantic_weight,
        wfst_lm_weight,
        max_query_edit_distance,
        "MCP tool invoked",
    );

    let dedupe_worktrees = params.dedupe_worktrees.unwrap_or(false);
    let ef_search = ctx.config().load().vector.ef_search;
    let text_leg_timeout_ms = ctx.config().load().database.hybrid_text_leg_timeout_ms;

    // A leg runs only when its weight is > 0 (a zero-weight leg contributes
    // nothing to the RRF). The text and semantic legs run CONCURRENTLY via
    // `tokio::join!` — deliberately NOT `try_join!`, which would short-circuit
    // on the first error. Each leg owns its failure handling: on error or
    // timeout it logs and yields an empty ranked list plus a `LegStatus`, so
    // the tool degrades to whatever succeeded instead of failing wholesale.
    // (The previous fail-fast `?` on either leg aborted the entire tool when a
    // single leg hit a Postgres `statement_timeout` — the bug being fixed.)
    let run_text = bm25_weight > 0.0;
    let run_semantic = semantic_weight > 0.0;

    let text_fut = async {
        if !run_text {
            return (Vec::new(), LegStatus::Skipped);
        }
        // The bounded variant scopes a tight `SET LOCAL statement_timeout` to
        // its own transaction so a cold / write-contended GIN index can't burn
        // the daemon-wide 30s ceiling. The outer `tokio::time::timeout` (SQL
        // budget + 1s margin) also bounds connection-acquire / network stalls.
        let wall = std::time::Duration::from_millis(u64::from(text_leg_timeout_ms) + 1_000);
        match tokio::time::timeout(
            wall,
            ctx.db().text_search_bounded(
                &params.query,
                limit * 2, // fetch more for fusion
                params.language.as_deref(),
                dedupe_worktrees,
                text_leg_timeout_ms,
            ),
        )
        .await
        {
            Ok(Ok(r)) => (r, LegStatus::Ok),
            Ok(Err(e)) => {
                warn!(leg = "text", error = %e, "hybrid_search text leg failed; degrading");
                (Vec::new(), LegStatus::Error)
            }
            Err(_) => {
                warn!(
                    leg = "text",
                    timeout_ms = text_leg_timeout_ms,
                    "hybrid_search text leg timed out; degrading"
                );
                (Vec::new(), LegStatus::Timeout)
            }
        }
    };

    let semantic_fut = async {
        if !run_semantic {
            return (Vec::new(), LegStatus::Skipped);
        }
        let embedding = match ctx.embed().embed_query(&params.query).await {
            Ok(e) => e,
            Err(e) => {
                warn!(leg = "semantic", error = %e, "hybrid_search embedding failed; degrading");
                return (Vec::new(), LegStatus::Error);
            }
        };
        match ctx
            .db()
            .semantic_search(
                &embedding,
                limit * 2,
                params.language.as_deref(),
                params.project.as_deref(),
                ef_search,
                dedupe_worktrees,
            )
            .await
        {
            Ok(r) => (r, LegStatus::Ok),
            Err(e) => {
                warn!(leg = "semantic", error = %e, "hybrid_search semantic leg failed; degrading");
                (Vec::new(), LegStatus::Error)
            }
        }
    };

    let ((text_results, text_status), (semantic_results, semantic_status)) =
        tokio::join!(text_fut, semantic_fut);

    // Third RRF leg — WFST lattice + per-project HybridLM rescoring.
    //
    // Activates iff: (a) the user did not opt out via wfst_lm_weight=0;
    // (b) a project name is supplied; (c) the per-project HybridLM model
    // file exists on disk (populated by the `ngram-lm-train` cron).
    // It is best-effort: any miss falls through to legacy 2-leg fusion and is
    // reported as `skipped` (never an error — this preserves the "no
    // per-project model" baseline). It runs after the join because it depends
    // on neither leg's results.
    let (wfst_rewritten_results, wfst_rewritten_query, legs_fused, wfst_status) =
        if wfst_lm_weight > 0.0 && params.project.is_some() {
            match try_third_leg(
                ctx,
                &params,
                limit,
                ef_search,
                dedupe_worktrees,
                wfst_lm_weight,
                max_query_edit_distance,
            )
            .await
            {
                Some((results, rewritten_query)) if !results.is_empty() => {
                    (results, Some(rewritten_query), 3u8, LegStatus::Ok)
                }
                _ => (Vec::new(), None, 2u8, LegStatus::Skipped),
            }
        } else {
            (Vec::new(), None, 2u8, LegStatus::Skipped)
        };

    // A text/semantic leg error or timeout degrades the result (the tool still
    // returns the surviving legs' hits) rather than failing. The optional WFST
    // leg never counts as degraded.
    let degraded = matches!(text_status, LegStatus::Error | LegStatus::Timeout)
        || matches!(semantic_status, LegStatus::Error | LegStatus::Timeout);

    let semantic_rewritten_weight = if legs_fused == 3 {
        semantic_weight
    } else {
        0.0
    };

    // Reciprocal Rank Fusion. See `RRF_K` and `rrf_score` above.
    let mut rrf_scores: std::collections::HashMap<String, (f64, serde_json::Value)> =
        std::collections::HashMap::new();

    // Score text search results
    for (rank, result) in text_results.iter().enumerate() {
        let key = format!("text:{}:{}", result.relative_path, rank);
        let rrf = rrf_score(bm25_weight, RRF_K, rank);
        let snippet = result.content.as_deref().unwrap_or("");
        let entry = rrf_scores.entry(key).or_insert((
            0.0,
            serde_json::json!({
                "path": result.path,
                "relative_path": result.relative_path,
                "snippet": truncate(snippet, 300),
                "language": result.language,
                "source": "text",
            }),
        ));
        entry.0 += rrf;
    }

    // Score semantic search results
    for (rank, result) in semantic_results.iter().enumerate() {
        let key = format!("semantic:{}:{}", result.relative_path, result.start_line);
        let rrf = rrf_score(semantic_weight, RRF_K, rank);
        let entry = rrf_scores.entry(key).or_insert((
            0.0,
            serde_json::json!({
                "path": result.path,
                "relative_path": result.relative_path,
                "project_name": result.project_name,
                "start_line": result.start_line,
                "end_line": result.end_line,
                "snippet": truncate(&result.chunk_content, 300),
                "language": result.language,
                "source": "semantic",
            }),
        ));
        entry.0 += rrf;
    }

    // Third RRF leg — semantic search on the WFST-rewritten query.
    for (rank, result) in wfst_rewritten_results.iter().enumerate() {
        let key = format!(
            "wfst_rewritten:{}:{}",
            result.relative_path, result.start_line
        );
        let rrf = rrf_score(semantic_rewritten_weight, RRF_K, rank);
        let entry = rrf_scores.entry(key).or_insert((
            0.0,
            serde_json::json!({
                "path": result.path,
                "relative_path": result.relative_path,
                "project_name": result.project_name,
                "start_line": result.start_line,
                "end_line": result.end_line,
                "snippet": truncate(&result.chunk_content, 300),
                "language": result.language,
                "source": "wfst_rewritten",
            }),
        ));
        entry.0 += rrf;
    }

    // Sort by RRF score and take top results
    let mut fused: Vec<serde_json::Value> = rrf_scores
        .into_iter()
        .map(|(_, (score, mut val))| {
            if let Some(o) = val.as_object_mut() {
                o.insert(
                    "rrf_score".to_string(),
                    serde_json::json!(format!("{:.6}", score)),
                );
            }
            val
        })
        .collect();

    fused.sort_by(|a, b| {
        let sa: f64 = a["rrf_score"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        let sb: f64 = b["rrf_score"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    fused.truncate(limit as usize);

    // Shadow-ASR channel (Phase D2b): workspace-wide effect distribution.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT se.effect, COUNT(*)::int8
             FROM symbol_effects se
             GROUP BY se.effect
             ORDER BY se.effect",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        rows.into_iter()
            .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
            .collect()
    })
    .await;

    let result = serde_json::json!({
        "effect_breakdown": effect_breakdown,
        "query": params.query,
        "project": params.project,
        "language": params.language,
        "bm25_weight": bm25_weight,
        "semantic_weight": semantic_weight,
        "wfst_lm_weight": wfst_lm_weight,
        "max_query_edit_distance": max_query_edit_distance,
        "text_results": text_results.len(),
        "semantic_results": semantic_results.len(),
        "wfst_rewritten_results": wfst_rewritten_results.len(),
        "wfst_rewritten_query": wfst_rewritten_query,
        "legs_fused": legs_fused,
        "degraded": degraded,
        "leg_status": {
            "text": text_status.label(),
            "semantic": semantic_status.label(),
            "wfst": wfst_status.label(),
        },
        "fused_count": fused.len(),
        "results": fused,
        "guidance": "RRF combines keyword precision with semantic recall. \
                     Increase bm25_weight for exact-match queries (error messages, function names). \
                     Increase semantic_weight for conceptual queries (design patterns, workflows). \
                     A third leg (WFST lattice + HybridLM-rescored query) activates when \
                     wfst_lm_weight > 0 and the per-project HybridLM model file is present. \
                     Legs run independently: when `degraded` is true a leg errored or timed out \
                     (see `leg_status`) and the results are partial — retry shortly.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "hybrid_search",
        results = fused.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// Third RRF leg. Rewrites the query via the per-project WFST lattice
/// (HybridLM rescoring on top), then re-runs semantic search on the
/// rewritten query. Returns the rewritten semantic results and the
/// rewritten query string. Returns `None` on any failure (HybridLM
/// not trained, no fuzzy vocabulary, re-embed failure) so the caller
/// falls back to legacy 2-leg fusion silently.
#[allow(clippy::too_many_arguments)]
async fn try_third_leg(
    ctx: &SystemContext,
    params: &crate::mcp::server::HybridSearchParams,
    limit: i32,
    ef_search: i32,
    dedupe_worktrees: bool,
    wfst_lm_weight: f64,
    max_query_edit_distance: usize,
) -> Option<(Vec<crate::db::queries::SearchResult>, String)> {
    let cfg_guard = ctx.config().load();
    let data_dir = cfg_guard.fuzzy.data_dir.clone();
    let project_name = params.project.as_ref()?;

    let model_path = crate::cron::ngram_lm_train::model_path_for(&data_dir, project_name);
    if !model_path.exists() {
        debug!(
            tool = "hybrid_search",
            model_path = %model_path.display(),
            "third leg skipped: per-project HybridLM model not present"
        );
        return None;
    }

    let lm = match crate::wfst::hybrid_lm::PgmcpHybridLm::open(&model_path) {
        Ok(lm) => lm,
        Err(e) => {
            warn!(error = %e, "third leg skipped: HybridLM load failed");
            return None;
        }
    };

    // P14.5 — pull candidates from the persistent symbol trie
    // (PersistentARTrieChar-backed) instead of rebuilding a
    // DynamicDawgChar per call from a PG SELECT. Lazy-warmed via
    // `open_symbol_trie`'s `rebuild_symbols` call when the trie
    // doesn't yet exist.
    let fuzzy_idx = match crate::fuzzy::sync::open_symbol_trie(ctx, project_name).await {
        Ok(idx) => idx,
        Err(e) => {
            warn!(
                project = %project_name,
                error = ?e,
                "third leg skipped: symbol trie open failed"
            );
            return None;
        }
    };
    if fuzzy_idx.is_empty() {
        debug!(
            tool = "hybrid_search",
            project = %project_name,
            "third leg skipped: project has no symbol vocabulary"
        );
        return None;
    }

    let rewritten = crate::wfst::query_rescore::rewrite_query(
        &params.query,
        max_query_edit_distance,
        1.0,
        wfst_lm_weight,
        cfg_guard.fuzzy.phonetic_cost_weight,
        cfg_guard.fuzzy.phonetic_max_total_cost,
        &fuzzy_idx,
        Some(&lm),
    );
    if !rewritten.changed {
        debug!(
            tool = "hybrid_search",
            "third leg skipped: rewrite unchanged"
        );
        return None;
    }

    let rewritten_embedding = ctx.embed().embed_query(&rewritten.rewritten).await.ok()?;
    let results = ctx
        .db()
        .semantic_search(
            &rewritten_embedding,
            limit * 2,
            params.language.as_deref(),
            params.project.as_deref(),
            ef_search,
            dedupe_worktrees,
        )
        .await
        .ok()?;

    Some((results, rewritten.rewritten))
}
