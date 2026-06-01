//! REST API handlers for the pgmcp daemon.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use super::ApiState;
use crate::db::queries::{StatusSnapshot, status_snapshot};

// ============================================================================
// GET /health — Cheap liveness probe (no DB queries, no model touch)
// ============================================================================

/// Lightweight liveness probe for k8s probes, systemd watchdogs, uptime
/// monitors, and the `~/.claude/hooks/pgmcp-*.sh` PreToolUse hooks
/// (which check this with a 300 ms timeout before deciding whether to
/// inject pgmcp context). Reads only an atomic phase from the
/// `DaemonLifecycle` — does not touch the DB or any worker pool.
///
/// 200 OK when the daemon is **serving-ready** — the DB pool is up (migrations
/// ran before the listener bound) AND ≥1 embedder worker has loaded its model —
/// else 503 SERVICE_UNAVAILABLE. Serving-readiness is deliberately decoupled
/// from the `Ready` *phase* (which means the initial file scan has finished):
/// search and RAG can be answered as soon as the service is able to, during the
/// initial scan, rather than waiting the whole scan out. The body reports
/// `phase` for index-progress visibility plus the readiness breakdown.
///
/// Intended to be polled at high frequency. Distinct from `/api/status`,
/// which returns a rich snapshot but issues ~10 SQL `COUNT(*)` queries.
pub async fn health(State(state): State<ApiState>) -> impl IntoResponse {
    let db_ready = state.db.pool().is_some();
    let embedder_ready = state.query_embedder.is_ready();
    let serving_ready = db_ready && embedder_ready;
    let body = Json(serde_json::json!({
        "phase": state.lifecycle.current().label(),
        "serving_ready": serving_ready,
        "db_ready": db_ready,
        "embedder_ready": embedder_ready,
        "ready_workers": state.query_embedder.ready_workers(),
    }));
    if serving_ready {
        (StatusCode::OK, body)
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, body)
    }
}

// ============================================================================
// POST /api/grep — Cross-project regex grep (REST mirror of mcp__pgmcp__grep)
// ============================================================================

/// Used by the `~/.claude/hooks/pgmcp-grep-companion.sh` PreToolUse hook
/// when the model issues a broad-path `Grep`. Hook calls this and injects
/// pgmcp's cross-project hits into the model's context alongside the
/// native `Grep` result.
#[derive(Debug, Deserialize)]
pub struct GrepRequest {
    pub pattern: String,
    pub glob: Option<String>,
    pub limit: Option<i32>,
}

#[derive(Debug, Serialize)]
pub struct GrepResponse {
    pub results: Vec<crate::db::queries::GrepResult>,
    pub truncated: bool,
}

pub async fn grep(
    State(state): State<ApiState>,
    Json(req): Json<GrepRequest>,
) -> Result<Json<GrepResponse>, (StatusCode, String)> {
    // Clamp limit to [1, 50] — the hook caps its own injection at 10, but
    // give a small buffer for direct callers.
    let limit = req.limit.unwrap_or(10).clamp(1, 50);

    // The /api/grep endpoint is consumed by ~/.claude/hooks/pgmcp-grep-companion.sh
    // which doesn't currently expose dedupe; default false preserves
    // existing behavior. The hook can opt in later via a query param.
    let results = state
        .db
        .grep_search(&req.pattern, req.glob.as_deref(), limit, false)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("grep_search failed: {}", e),
            )
        })?;

    let truncated = results.len() == limit as usize;
    Ok(Json(GrepResponse { results, truncated }))
}

// ============================================================================
// POST /api/file_envelope — File metadata for the read-context hook
// ============================================================================

/// Compact envelope returned to `~/.claude/hooks/pgmcp-read-context.sh`
/// when the model is about to `Read` a file: language, line count,
/// last_indexed_at. Future expansion will include centrality_rank,
/// top_topics, top_coupled_files, and recent_commits — for now it returns
/// what the trait already exposes via `file_info`.
#[derive(Debug, Deserialize)]
pub struct FileEnvelopeRequest {
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct FileEnvelopeResponse {
    pub found: bool,
    pub info: Option<crate::db::queries::FileInfo>,
}

pub async fn file_envelope(
    State(state): State<ApiState>,
    Json(req): Json<FileEnvelopeRequest>,
) -> Result<Json<FileEnvelopeResponse>, (StatusCode, String)> {
    let info = state.db.file_info(&req.path).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("file_info failed: {}", e),
        )
    })?;

    Ok(Json(FileEnvelopeResponse {
        found: info.is_some(),
        info,
    }))
}

// ============================================================================
// POST /api/search — Semantic search
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub limit: Option<i32>,
    pub project: Option<String>,
    pub language: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResultItem>,
    /// True when the cross-encoder rerank stage ran (gated by `[api]
    /// rerank_hook`). Lets the hook / telemetry see whether reranking fired
    /// vs. the RRF-only fallback.
    pub rerank_used: bool,
    /// True when the ColBERT late-interaction (MaxSim) rerank stage ran (gated
    /// by `[api] colbert_rerank` and a backbone with a ColBERT head). Applied
    /// before the cross-encoder when both are enabled. (Phase 2.5)
    pub colbert_used: bool,
}

#[derive(Debug, Serialize)]
pub struct SearchResultItem {
    pub file_path: String,
    pub chunk: String,
    pub similarity: f64,
    pub language: String,
}

pub async fn search(
    State(state): State<ApiState>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, (StatusCode, String)> {
    let limit = req.limit.unwrap_or(5);

    // Cold-start fast-fail: if no embedder worker has finished loading its model
    // yet, return 503 immediately rather than parking the request in the bounded
    // query channel until one warms up — which would blow the RAG hook's
    // ~300ms–3s budget. The hook treats 503 as "skip pgmcp this turn" and falls
    // back cleanly.
    if !state.query_embedder.is_ready() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "embedder warming up".to_string(),
        ));
    }

    // Embed the query — dense + (when the backbone has a sparse head) the
    // BGE-M3 learned-sparse vector that feeds the optional sparse RRF leg.
    let query_rep = state
        .query_embedder
        .embed_query_hybrid(req.query.clone())
        .await
        .map_err(|e| {
            state
                .stats
                .rag_search_failures_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Embedding failed: {}", e),
            )
        })?;
    let embedding = query_rep.dense;
    let query_sparse = query_rep.sparse;

    // The /api/search endpoint is consumed by ~/.claude/hooks/pgmcp-rag.sh
    // (UserPromptSubmit). It always fuses dense + BM25 at chunk level via RRF
    // (cheap, no extra model); the cross-encoder rerank stage is opt-in.
    let (
        ef_search,
        rerank_candidates,
        rerank_enabled,
        colbert_enabled,
        colbert_candidates,
        mmr_lambda,
        recency_half_life_days,
    ) = {
        let cfg = state.config.load();
        (
            cfg.vector.ef_search,
            cfg.api.rerank_candidates.max(limit),
            cfg.api.rerank_hook && state.reranker.read().is_some(),
            cfg.api.colbert_rerank,
            cfg.api.colbert_candidates.max(limit),
            cfg.api.mmr_lambda,
            cfg.api.recency_half_life_days,
        )
    };
    let rerank_ext_enabled = mmr_lambda > 0.0 || recency_half_life_days > 0.0;

    // Fetch enough candidates to feed whichever rerank stages are active.
    // ColBERT casts the widest net (cheap MaxSim), then the cross-encoder, then
    // the bare `limit`. Per-leg pool is 2× the deepest fetch.
    let fetch_n = if colbert_enabled {
        colbert_candidates.max(if rerank_enabled {
            rerank_candidates
        } else {
            limit
        })
    } else if rerank_enabled {
        rerank_candidates
    } else {
        limit
    };
    // MMR/recency need a candidate pool wider than `limit` to diversify over.
    let fetch_n = if rerank_ext_enabled {
        fetch_n.max((limit * 4).clamp(20, 100))
    } else {
        fetch_n
    };
    let per_leg = (fetch_n * 2).clamp(20, 200);

    let pool = state.db.pool().ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "no database pool".to_string(),
        )
    })?;

    let mut results = crate::db::queries::hybrid_search_chunks(
        pool,
        &req.query,
        &embedding,
        fetch_n,
        per_leg,
        req.language.as_deref(),
        req.project.as_deref(),
        ef_search,
        query_sparse.as_ref(),
    )
    .await
    .map_err(|e| {
        state
            .stats
            .rag_search_failures_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Search failed: {}", e),
        )
    })?;

    // Optional ColBERT late-interaction (MaxSim) rerank. Recomputes per-token
    // matrices for the query + the top candidates with the resident BGE-M3
    // ColBERT head (no extra VRAM, unlike the cross-encoder) and reorders the
    // candidate pool in place, so a subsequent cross-encoder pass operates on
    // the improved order. Any failure (no ColBERT head, embed error) leaves the
    // RRF order untouched — never hard-fail the hook.
    let mut colbert_used = false;
    if colbert_enabled && results.len() > 1 {
        let n = (colbert_candidates as usize).min(results.len());
        // [query, cand_0, .., cand_{n-1}] share one forward pass.
        let mut texts: Vec<String> = Vec::with_capacity(n + 1);
        texts.push(req.query.clone());
        texts.extend(results[..n].iter().map(|r| r.chunk_content.clone()));
        match state.query_embedder.embed_colbert_batch(texts).await {
            Ok(mats) => {
                // mats[0] = query tokens; mats[1..=n] = candidate tokens.
                match mats.split_first() {
                    Some((Some(query_tokens), cand_mats)) => {
                        // Score each candidate; missing matrices sort last.
                        let mut scored: Vec<(usize, f32)> = (0..n)
                            .map(|i| {
                                let score = cand_mats
                                    .get(i)
                                    .and_then(|m| m.as_ref())
                                    .map(|doc| {
                                        crate::embed::model::colbert_maxsim(query_tokens, doc)
                                    })
                                    .unwrap_or(f32::NEG_INFINITY);
                                (i, score)
                            })
                            .collect();
                        // Descending by MaxSim; stable so RRF order breaks ties.
                        scored.sort_by(|a, b| {
                            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
                        });
                        // Reorder the top-n in place; the tail keeps RRF order.
                        let head: Vec<_> = scored
                            .into_iter()
                            .map(|(i, _)| results[i].clone())
                            .collect();
                        results.splice(..n, head);
                        colbert_used = true;
                    }
                    _ => tracing::debug!("ColBERT rerank skipped: backbone has no ColBERT head"),
                }
            }
            Err(e) => tracing::warn!(error = %e, "ColBERT rerank failed; using RRF order"),
        }
    }

    // Optional cross-encoder rerank of the fused candidates. The candle forward
    // is synchronous, so it runs on a blocking thread. Any failure falls back
    // to the RRF order — the hook must never hard-fail on a rerank error.
    let mut rerank_hits: Vec<crate::reranker::RerankHit> = Vec::new();
    let mut rerank_used = false;
    // Snapshot the hot-swappable reranker handle, releasing the RwLock guard
    // before the spawn_blocking await below so the handler future stays Send.
    let reranker_opt = state.reranker.read().clone();
    if rerank_enabled
        && results.len() > 1
        && let Some(reranker) = reranker_opt
    {
        let query = req.query.clone();
        let cands: Vec<String> = results.iter().map(|r| r.chunk_content.clone()).collect();
        match tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = cands.iter().map(|s| s.as_str()).collect();
            reranker.rerank(&query, &refs)
        })
        .await
        {
            Ok(Ok(hits)) => {
                rerank_hits = hits;
                rerank_used = true;
            }
            Ok(Err(e)) => tracing::warn!(error = %e, "hook rerank failed; using RRF order"),
            Err(e) => {
                tracing::warn!(error = %e, "hook rerank task join failed; using RRF order")
            }
        }
    }

    // Base candidate order (index into `results`, base relevance) from whatever
    // the prior stages left: cross-encoder order if it ran, else the
    // RRF/ColBERT order with its fused score as relevance.
    let mut order: Vec<(usize, f64)> = if rerank_used {
        rerank_hits
            .iter()
            .filter_map(|h| {
                results
                    .get(h.original_index)
                    .map(|_| (h.original_index, h.score as f64))
            })
            .collect()
    } else {
        (0..results.len())
            .map(|i| (i, results[i].score.unwrap_or(0.0)))
            .collect()
    };

    // Phase 4.2: optional recency prior + MMR diversity over the candidate pool,
    // as the final selection stage. Recency reweights relevance by blame_date;
    // MMR then picks a diverse top-`limit`. Any feature-fetch failure leaves the
    // base order untouched.
    if rerank_ext_enabled && order.len() > 1 {
        let chunk_ids: Vec<i64> = order
            .iter()
            .filter_map(|&(i, _)| results.get(i).and_then(|r| r.chunk_id))
            .collect();
        if !chunk_ids.is_empty()
            && let Ok(feats) =
                crate::db::queries::chunk_rerank_features(pool, &chunk_ids, embedding.len()).await
        {
            let mut emb_by: std::collections::HashMap<i64, Vec<f32>> =
                std::collections::HashMap::new();
            let mut date_by: std::collections::HashMap<i64, chrono::DateTime<chrono::Utc>> =
                std::collections::HashMap::new();
            for f in feats {
                if let Some(v) = f.embedding {
                    emb_by.insert(f.chunk_id, v.as_slice().to_vec());
                }
                if let Some(d) = f.blame_date {
                    date_by.insert(f.chunk_id, d);
                }
            }
            if recency_half_life_days > 0.0 {
                let now = chrono::Utc::now();
                for (i, rel) in order.iter_mut() {
                    if let Some(cid) = results.get(*i).and_then(|r| r.chunk_id)
                        && let Some(d) = date_by.get(&cid)
                    {
                        let age_days = (now - *d).num_seconds().max(0) as f64 / 86_400.0;
                        *rel *= crate::embed::rerank_ext::recency_multiplier(
                            age_days,
                            recency_half_life_days,
                        );
                    }
                }
            }
            let selected: Vec<usize> = if mmr_lambda > 0.0 {
                let embs: Vec<Vec<f32>> = order
                    .iter()
                    .map(|&(i, _)| {
                        results
                            .get(i)
                            .and_then(|r| r.chunk_id)
                            .and_then(|c| emb_by.get(&c).cloned())
                            .unwrap_or_default()
                    })
                    .collect();
                let rels: Vec<f64> = order.iter().map(|&(_, r)| r).collect();
                crate::embed::rerank_ext::mmr_select(&embs, &rels, mmr_lambda, limit as usize)
            } else {
                let mut pos: Vec<usize> = (0..order.len()).collect();
                pos.sort_by(|&a, &b| {
                    order[b]
                        .1
                        .partial_cmp(&order[a].1)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                pos.truncate(limit as usize);
                pos
            };
            let new_order: Vec<(usize, f64)> = selected
                .into_iter()
                .filter_map(|p| order.get(p).copied())
                .collect();
            order = new_order;
        }
    }

    let items: Vec<SearchResultItem> = order
        .iter()
        .take(limit as usize)
        .filter_map(|&(i, score)| {
            results.get(i).map(|r| SearchResultItem {
                file_path: r.path.clone(),
                chunk: r.chunk_content.clone(),
                similarity: score,
                language: r.language.clone(),
            })
        })
        .collect();

    Ok(Json(SearchResponse {
        results: items,
        rerank_used,
        colbert_used,
    }))
}

// ============================================================================
// POST /api/session/observe — Session-mandate observation + re-injection
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct ObserveRequest {
    pub session_id: uuid::Uuid,
    pub cwd: String,
    pub prompt: String,
    #[serde(default = "default_true")]
    pub include_rag: bool,
    pub rag_limit: Option<i32>,
    /// Reporting agent id (e.g. "claude-code"). Attributed to the memory
    /// scope so the multi-agent shared-memory `agent_id` dimension is
    /// populated. Optional — defaults to workspace scope when absent
    /// (no regression for hooks that don't send it).
    #[serde(default)]
    pub agent_id: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
pub struct ObserveResponse {
    pub session_id: uuid::Uuid,
    pub prompt_id: i64,
    pub extracted: Vec<crate::sessions::ExtractedMandate>,
    pub active_mandates: Vec<crate::sessions::SessionMandate>,
    pub rag_hits: Vec<SearchResultItem>,
    pub additional_context: String,
}

pub async fn session_observe(
    State(state): State<ApiState>,
    Json(req): Json<ObserveRequest>,
) -> Result<Json<ObserveResponse>, (StatusCode, String)> {
    let pool = state.db.pool().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "raw pool unavailable".to_string(),
    ))?;

    // Resolve project_id from cwd (longest-prefix match).
    let project = state.db.find_project_by_cwd(&req.cwd).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("project lookup failed: {}", e),
        )
    })?;
    let project_id = project.as_ref().map(|p| p.id);

    crate::sessions::upsert_session(pool, req.session_id, &req.cwd, project_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("upsert_session failed: {}", e),
            )
        })?;

    let sha256 = crate::sessions::prompt_sha256(&req.prompt);

    // Embed the prompt for cross-session retrieval (and to populate the
    // vector column on the row we're about to insert).
    let embedding = state
        .query_embedder
        .embed_query(req.prompt.clone())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Embedding failed: {}", e),
            )
        })?;

    let prompt_id = crate::sessions::insert_prompt(
        pool,
        req.session_id,
        &req.prompt,
        &sha256,
        Some(&embedding),
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("insert_prompt failed: {}", e),
        )
    })?;

    let extracted = crate::sessions::extract_mandates(&req.prompt, Some(&req.cwd));
    for m in &extracted {
        match crate::sessions::upsert_mandate(pool, req.session_id, prompt_id, m).await {
            Ok(keeper_id) => {
                // Phase 0: mark active near-duplicates (Levenshtein ≤ 3 on
                // `lower(imperative)`) as Superseded so the active list stays
                // scannable. Survives `upsert_mandate`'s exact-match dedupe.
                match crate::sessions::mark_near_duplicate_superseded(
                    pool,
                    req.session_id,
                    keeper_id,
                    m.polarity.as_str(),
                    &m.imperative,
                    3,
                )
                .await
                {
                    Ok(count) if count > 0 => {
                        state
                            .stats
                            .memory_mandate_supersessions
                            .fetch_add(count, std::sync::atomic::Ordering::Relaxed);
                        tracing::debug!(
                            session = %req.session_id,
                            polarity = m.polarity.as_str(),
                            keeper_id,
                            count,
                            "marked near-duplicate mandates as superseded",
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "mark_near_duplicate_superseded failed")
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "upsert_mandate failed"),
        }
    }

    let active = crate::sessions::list_active_mandates(pool, Some(req.session_id), None, 20)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_active_mandates failed: {}", e),
            )
        })?;

    // Memory-server Phase 4 Stage B: spawn the LLM extractor in the
    // background. Does NOT block the HTTP response — the inline path
    // (mandates + RAG) returns to the caller immediately while the
    // extractor runs on the runtime's blocking-pool thread.
    // Snapshot the hot-swappable extractor handle, releasing the RwLock guard
    // before any await so the handler future stays Send.
    let extractor_opt = state.llm_extractor.read().clone();
    if let Some(extractor) = extractor_opt {
        let pool_clone = pool.clone();
        let stats_clone = Arc::clone(&state.stats);
        let debounce_clone = Arc::clone(&state.extractor_debounce);
        let config_snapshot = state.config.load();
        let worker_config = crate::llm::extractor_worker::ExtractorWorkerConfig {
            debounce: std::time::Duration::from_secs(
                config_snapshot.memory.extractor.inline_debounce_secs,
            ),
            ..crate::llm::extractor_worker::ExtractorWorkerConfig::default()
        };
        // Resolve project_id by longest-cwd-prefix (best-effort; None on miss).
        let project_id = crate::db::queries::find_project_by_cwd(pool, &req.cwd)
            .await
            .ok()
            .flatten()
            .map(|p| p.id);
        let job = crate::llm::extractor_worker::ExtractorJob {
            session_id: req.session_id,
            source_prompt_id: prompt_id,
            project_id,
            agent_id: req.agent_id.clone(), // A6: from the hook / MCP clientInfo
            user_id: std::env::var("USER").ok(),
            prompt_text: req.prompt.clone(),
        };
        tokio::spawn(async move {
            crate::llm::extractor_worker::run_extraction_for_prompt(
                pool_clone,
                stats_clone,
                extractor,
                debounce_clone,
                worker_config,
                job,
            )
            .await;
        });
    }

    // Optional RAG hits using the existing semantic_search path.
    let mut rag_hits: Vec<SearchResultItem> = Vec::new();
    if req.include_rag {
        let limit = req.rag_limit.unwrap_or(5).clamp(1, 20);
        let ef_search = state.config.load().vector.ef_search;
        if let Ok(hits) = state
            .db
            .semantic_search(&embedding, limit, None, None, ef_search, false)
            .await
        {
            rag_hits = hits
                .into_iter()
                .map(|r| SearchResultItem {
                    file_path: r.path,
                    chunk: r.chunk_content,
                    similarity: r.score.unwrap_or(0.0),
                    language: r.language,
                })
                .collect();
        }
    }

    // Render the combined `additional_context` Markdown block (≤ 2 KB).
    let mut additional_context = crate::sessions::render_session_mandates_md(&active, 2048);
    if !rag_hits.is_empty() {
        additional_context.push_str("\n## Relevant indexed code (pgmcp RAG)\n\n");
        let budget_remaining = 2048usize.saturating_sub(additional_context.len());
        let mut used = 0;
        for hit in &rag_hits {
            let line = format!("- `{}` (similarity {:.2})\n", hit.file_path, hit.similarity);
            if used + line.len() > budget_remaining {
                break;
            }
            additional_context.push_str(&line);
            used += line.len();
        }
    }

    // Read-before-act (Part A): inject peer best practices (workspace ∪
    // project scope, G1). No-op unless [a2a] inject_best_practices = true.
    let bp = crate::a2a::best_practices::retrieve_for_prompt(
        &state.system_ctx,
        project_id,
        &req.prompt,
        512,
    )
    .await;
    if !bp.is_empty() && additional_context.len() + bp.len() < 2048 {
        additional_context.push('\n');
        additional_context.push_str(&bp);
    }

    // JIT adoption nudge (Claude-only — only clients running the observe hook
    // reach this path). A single, deduplicated, budget-bounded suggestion toward
    // an under-used tool family; logged to nudge_emissions for the Phase-3
    // conversion metric and the per-(session, family) rate limit. Off unless
    // [nudges] enabled = true.
    let nudges_cfg = state.config.load().nudges.clone();
    if nudges_cfg.enabled
        && let Some(family) = crate::sessions::classify_tool_suggestion(&req.prompt)
        && let Some(pool) = state.system_ctx.db().pool()
    {
        let session_key = req.session_id.to_string();
        let family_key = crate::sessions::tool_family_key(family);
        let brief = req
            .agent_id
            .as_deref()
            .map(|a| a.contains("codex"))
            .unwrap_or(false);
        let nudge = crate::sessions::tool_suggestion_nudge(family, brief);
        let fits = additional_context.len() + nudge.len() + 1 < 2048;
        let recently = crate::sessions::recently_nudged(
            pool,
            &session_key,
            family_key,
            nudges_cfg.ttl_secs as i64,
        )
        .await
        .unwrap_or(false);
        let count = crate::sessions::session_nudge_count(pool, &session_key, family_key)
            .await
            .unwrap_or(i64::MAX);
        if fits && !recently && count < nudges_cfg.max_per_session as i64 {
            additional_context.push('\n');
            additional_context.push_str(&nudge);
            // Fire-and-forget so the emission log never blocks the response.
            let pool = pool.clone();
            let client = req.agent_id.clone();
            tokio::spawn(async move {
                let _ = crate::sessions::insert_nudge_emission(
                    &pool,
                    &session_key,
                    Some(prompt_id),
                    family_key,
                    "prompt",
                    client.as_deref(),
                    project_id,
                )
                .await;
            });
        }
    }

    // Phase 4: proactive digest. Rides this same `additional_context` channel
    // (after the nudge block), surfacing tracker/health/trend state. Daemon path,
    // so it passes `Some(&state.stats)` (HEALTH can include the cron-failure
    // signal). Read-only: SELECTs + the maybe_emit ledger insert. Off unless
    // [digest] enabled = true.
    let digest_cfg = state.config.load().digest.clone();
    if digest_cfg.enabled
        && digest_cfg.prompt
        && let Some(pool) = state.system_ctx.db().pool()
    {
        let digest =
            crate::digest::compose_digest(pool, project_id, Some(&state.stats), &digest_cfg).await;
        if !digest.is_empty() {
            let block = digest.render_markdown(digest_cfg.max_bytes);
            // Mirror the nudge `fits` check against the 2 KB additional_context
            // budget (the digest's own max_bytes already bounds `block`).
            let fits = !block.is_empty() && additional_context.len() + block.len() + 1 < 2048;
            let session_key = req.session_id.to_string();
            if fits
                && crate::digest::maybe_emit(
                    pool,
                    &session_key,
                    crate::digest::DigestChannel::Prompt,
                    project_id,
                    &digest_cfg,
                    &digest,
                )
                .await
            {
                additional_context.push('\n');
                additional_context.push_str(&block);

                // Optional outbound webhook (daemon-only, min-severity gated,
                // empty-URL default off) — fire-and-forget.
                crate::digest::webhook::post_webhook(
                    &digest_cfg,
                    crate::digest::DigestChannel::Prompt,
                    &digest,
                );
                // Optional pg_notify seam (default off; no SSE consumer built).
                if digest_cfg.pg_notify {
                    let pool = pool.clone();
                    let sk = session_key.clone();
                    let d = digest.clone();
                    tokio::spawn(async move {
                        let _ = crate::digest::notify_digest_ready(
                            &pool,
                            &sk,
                            crate::digest::DigestChannel::Prompt,
                            &d,
                        )
                        .await;
                    });
                }
            }
        }
    }

    Ok(Json(ObserveResponse {
        session_id: req.session_id,
        prompt_id,
        extracted,
        active_mandates: active,
        rag_hits,
        additional_context,
    }))
}

// ============================================================================
// GET /api/context?cwd=/path — Project context
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct ContextQuery {
    pub cwd: String,
    pub depth: Option<i32>,
}

#[derive(Debug, Serialize)]
pub struct ContextResponse {
    pub found: bool,
    pub project: Option<ProjectContext>,
    pub indexed_projects: Option<Vec<ProjectSummary>>,
}

#[derive(Debug, Serialize)]
pub struct ProjectContext {
    pub name: String,
    pub path: String,
    pub file_count: i64,
    pub last_scanned: Option<String>,
    pub languages: Vec<LanguageEntry>,
    pub tree: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct LanguageEntry {
    pub language: String,
    pub count: i64,
}

#[derive(Debug, Serialize)]
pub struct ProjectSummary {
    pub name: String,
    pub path: String,
    pub file_count: i64,
}

pub async fn context(
    State(state): State<ApiState>,
    Query(params): Query<ContextQuery>,
) -> Result<Json<ContextResponse>, (StatusCode, String)> {
    let depth = params.depth.unwrap_or(3);

    let cwd_normalized = if params.cwd.ends_with('/') {
        params.cwd.clone()
    } else {
        format!("{}/", params.cwd)
    };

    let project = state
        .db
        .find_project_by_cwd(&cwd_normalized)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Query failed: {}", e),
            )
        })?;

    match project {
        Some(p) => {
            let languages = state.db.language_summary(&p.name).await.map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Language query failed: {}", e),
                )
            })?;

            let tree = state.db.project_tree(&p.name, depth).await.map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Tree query failed: {}", e),
                )
            })?;

            Ok(Json(ContextResponse {
                found: true,
                project: Some(ProjectContext {
                    name: p.name,
                    path: p.path,
                    file_count: p.file_count.unwrap_or(0),
                    last_scanned: p
                        .last_scanned_at
                        .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string()),
                    languages: languages
                        .into_iter()
                        .map(|l| LanguageEntry {
                            language: l.language,
                            count: l.count,
                        })
                        .collect(),
                    tree,
                }),
                indexed_projects: None,
            }))
        }
        None => {
            let projects = state.db.list_projects().await.map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("List projects failed: {}", e),
                )
            })?;

            Ok(Json(ContextResponse {
                found: false,
                project: None,
                indexed_projects: Some(
                    projects
                        .into_iter()
                        .map(|p| ProjectSummary {
                            name: p.name,
                            path: p.path,
                            file_count: p.file_count.unwrap_or(0),
                        })
                        .collect(),
                ),
            }))
        }
    }
}

// ============================================================================
// POST /api/tracker/ingest_plan — auto-translate an agent plan into a tree
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct TrackerIngestRequest {
    pub plan_markdown: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub definition_slug: Option<String>,
}

/// Ingest an agent's plan markdown into a tracked `work_items` subtree. Resolves
/// the project from `cwd` (longest-prefix) when not given. This is the seam the
/// PostToolUse:ExitPlanMode hook POSTs to. Reuses the tool's `ingest_plan_core`.
pub async fn tracker_ingest_plan(
    State(state): State<ApiState>,
    Json(req): Json<TrackerIngestRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let project = match (&req.project, &req.cwd) {
        (Some(p), _) => Some(p.clone()),
        (None, Some(cwd)) => state
            .db
            .find_project_by_cwd(cwd)
            .await
            .ok()
            .flatten()
            .map(|p| p.name),
        _ => None,
    };
    let out = crate::mcp::tools::work_items::ingest_plan_core(
        &state.system_ctx,
        &req.plan_markdown,
        project.as_deref(),
        req.definition_slug.as_deref(),
    )
    .await
    .map_err(|e| (StatusCode::BAD_REQUEST, e.message.to_string()))?;
    Ok(Json(out))
}

// ============================================================================
// POST /api/tracker/record_evidence — trusted-source evidence (hooks/CI)
// ============================================================================

/// The trusted evidence-source whitelist. Only these `source` values may
/// satisfy the `→verified` gate (`verification_evidence.source` is also
/// CHECK-mirrored in `set_work_item_status`'s evidence query). An agent's
/// MCP-recorded evidence is `source='manual'` and is deliberately absent.
pub(crate) const TRUSTED_EVIDENCE_SOURCES: &[&str] = &[
    "ci",
    "stop_hook",
    "subagent_audit",
    "external_auditor",
    "user_signoff",
    "experiment",
];

/// Best-effort gatekeeper `→verified` after passing trusted evidence has been
/// recorded for `item_id`. Factored out of [`tracker_record_evidence`] so the
/// `/api/tracker/ci_evidence` path shares the exact same trust-critical
/// transition: the gatekeeper move succeeds ONLY when the item is in
/// `claimed_done`/`verifying` and EVERY required criterion now has a passing,
/// trusted-source evidence row (re-checked inside `set_work_item_status` →
/// `check_transition`). Returns whether the item is now `verified`. Errors are
/// swallowed (the evidence is already saved); a refusal simply leaves the item
/// where it was.
///
/// TRUST: this is the ONLY `Actor::Gatekeeper` path the REST surface exposes,
/// and it is reachable only after the caller has passed the `user_token` gate
/// and a TRUSTED `source`. `source` is recorded as the actor id on the
/// status-history row.
pub(crate) async fn try_auto_verify(pool: &sqlx::PgPool, item_id: i64, source: &str) -> bool {
    let ev = crate::db::queries::latest_passing_evidence_id(pool, item_id)
        .await
        .ok()
        .flatten();
    crate::db::queries::set_work_item_status(
        pool,
        item_id,
        crate::tracker::status::WorkItemStatus::Verified,
        crate::tracker::transition::Actor::Gatekeeper,
        Some(source),
        Some("auto-verify on trusted evidence"),
        ev,
        None,
    )
    .await
    .is_ok()
}

#[derive(Debug, Deserialize)]
pub struct TrackerEvidenceRequest {
    /// Must match `[tracker] user_token` — the credential that distinguishes a
    /// trusted producer (hook/CI) from the agent.
    pub token: String,
    pub criterion_id: i64,
    pub verdict: String,
    pub source: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub coverage_count: Option<i32>,
    #[serde(default)]
    pub coverage_total: Option<i32>,
    #[serde(default)]
    pub runner_identity: Option<String>,
    #[serde(default)]
    pub commit_sha: Option<String>,
    #[serde(default)]
    pub spec_sha256: Option<String>,
    #[serde(default)]
    pub detail_json: Option<String>,
}

/// Record TRUSTED-source verification evidence (the path agents cannot use — it
/// is token-gated and only accepts trusted sources). On passing evidence it
/// best-effort runs the gatekeeper `→verified` transition, closing the
/// verification loop for CI / the Stop-hook.
pub async fn tracker_record_evidence(
    State(state): State<ApiState>,
    Json(req): Json<TrackerEvidenceRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Credential gate (guard scoped so it is not held across an await).
    let token_ok = {
        let cfg = state.config.load();
        cfg.tracker
            .user_token
            .as_deref()
            .map(|t| t == req.token)
            .unwrap_or(false)
    };
    if !token_ok {
        return Err((
            StatusCode::FORBIDDEN,
            "invalid or missing tracker token (set [tracker] user_token)".to_string(),
        ));
    }
    if !TRUSTED_EVIDENCE_SOURCES.contains(&req.source.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("source must be one of {TRUSTED_EVIDENCE_SOURCES:?}"),
        ));
    }
    if !matches!(req.verdict.as_str(), "pass" | "fail" | "unknown" | "error") {
        return Err((
            StatusCode::BAD_REQUEST,
            "verdict must be pass|fail|unknown|error".to_string(),
        ));
    }
    let pool = state.db.pool().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "raw pool unavailable".to_string(),
    ))?;
    let detail = req.detail_json.clone().unwrap_or_else(|| "{}".to_string());
    if serde_json::from_str::<serde_json::Value>(&detail).is_err() {
        return Err((
            StatusCode::BAD_REQUEST,
            "detail_json must be valid JSON".to_string(),
        ));
    }
    let evidence_id = crate::db::queries::record_verification_evidence(
        pool,
        req.criterion_id,
        &req.verdict,
        &req.source,
        req.exit_code,
        req.coverage_count,
        req.coverage_total,
        req.runner_identity.as_deref(),
        None,
        req.commit_sha.as_deref(),
        req.spec_sha256.as_deref(),
        &detail,
    )
    .await
    .map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("record evidence failed (unknown criterion?): {e}"),
        )
    })?;

    // Best-effort auto-verify on passing evidence: the gatekeeper transition
    // succeeds only if the item is in claimed_done/verifying and every required
    // criterion now passes (errors are swallowed — the evidence is still saved).
    let mut verified = false;
    if req.verdict == "pass" {
        let item_id: Option<i64> =
            sqlx::query_scalar("SELECT item_id FROM acceptance_criteria WHERE id = $1")
                .bind(req.criterion_id)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten();
        if let Some(iid) = item_id {
            verified = try_auto_verify(pool, iid, &req.source).await;
        }
    }
    Ok(Json(serde_json::json!({
        "evidence_id": evidence_id,
        "source": req.source,
        "verified": verified,
    })))
}

// ============================================================================
// POST /api/tracker/ci_evidence — CI closes the loop by public_id
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct TrackerCiEvidenceRequest {
    /// Must match `[tracker] user_token` — the trusted-producer credential.
    pub token: String,
    /// The work item's `public_id` (CI knows the human id, not criterion ids).
    pub public_id: String,
    /// Verdict for the posted evidence: pass | fail | unknown | error.
    pub verdict: String,
    /// Evidence source — must be one of the TRUSTED set (typically `ci`).
    pub source: String,
    #[serde(default)]
    pub commit_sha: Option<String>,
    #[serde(default)]
    pub runner_identity: Option<String>,
    /// Target a single criterion by its `acceptance_uri` (most precise).
    #[serde(default)]
    pub criterion_uri: Option<String>,
    /// Else target every criterion of this `criterion_kind`.
    #[serde(default)]
    pub criterion_kind: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub coverage_count: Option<i32>,
    #[serde(default)]
    pub coverage_total: Option<i32>,
    #[serde(default)]
    pub detail_json: Option<String>,
}

/// Record TRUSTED-source CI evidence keyed by an item's `public_id` (rather than
/// a criterion id), post it to the selected acceptance criteria, then run the
/// SHARED [`try_auto_verify`] — the credential-gated `Actor::Gatekeeper` path
/// that flips `→verified` when every required criterion now passes. This is how
/// CI closes the verification loop: it is the ONLY way (besides the existing
/// `record_evidence`) to legitimately reach `verified`.
///
/// Criterion selection: by `criterion_uri` (matched against `acceptance_uri`) if
/// given; else every criterion of `criterion_kind`; else all `required`
/// criteria. Evidence is posted to each; the gatekeeper verify then re-checks
/// the full required set.
pub async fn tracker_ci_evidence(
    State(state): State<ApiState>,
    Json(req): Json<TrackerCiEvidenceRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Credential gate (scoped so the guard is not held across an await).
    let token_ok = {
        let cfg = state.config.load();
        cfg.tracker
            .user_token
            .as_deref()
            .map(|t| t == req.token)
            .unwrap_or(false)
    };
    if !token_ok {
        return Err((
            StatusCode::FORBIDDEN,
            "invalid or missing tracker token (set [tracker] user_token)".to_string(),
        ));
    }
    if !TRUSTED_EVIDENCE_SOURCES.contains(&req.source.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("source must be one of {TRUSTED_EVIDENCE_SOURCES:?}"),
        ));
    }
    if !matches!(req.verdict.as_str(), "pass" | "fail" | "unknown" | "error") {
        return Err((
            StatusCode::BAD_REQUEST,
            "verdict must be pass|fail|unknown|error".to_string(),
        ));
    }
    let pool = state.db.pool().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "raw pool unavailable".to_string(),
    ))?;
    let detail = req.detail_json.clone().unwrap_or_else(|| "{}".to_string());
    if serde_json::from_str::<serde_json::Value>(&detail).is_err() {
        return Err((
            StatusCode::BAD_REQUEST,
            "detail_json must be valid JSON".to_string(),
        ));
    }

    let item = crate::db::queries::get_work_item_by_public_id(pool, &req.public_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")))?
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("no work item '{}'", req.public_id),
        ))?;

    let criteria = crate::db::queries::list_acceptance_criteria(pool, item.id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")))?;
    // Select the target criteria: by uri, else by kind, else all required.
    let targets: Vec<&crate::db::queries::AcceptanceCriterionRow> = if let Some(uri) = req
        .criterion_uri
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        criteria
            .iter()
            .filter(|c| c.acceptance_uri.as_deref() == Some(uri))
            .collect()
    } else if let Some(kind) = req
        .criterion_kind
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        criteria
            .iter()
            .filter(|c| c.criterion_kind == kind)
            .collect()
    } else {
        criteria.iter().filter(|c| c.required).collect()
    };
    if targets.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "no matching acceptance criteria (add one via work_item_add_criterion, or check \
             criterion_uri/criterion_kind)"
                .to_string(),
        ));
    }

    let mut evidence_ids: Vec<i64> = Vec::with_capacity(targets.len());
    for c in &targets {
        let eid = crate::db::queries::record_verification_evidence(
            pool,
            c.id,
            &req.verdict,
            &req.source,
            req.exit_code,
            req.coverage_count,
            req.coverage_total,
            req.runner_identity.as_deref(),
            None,
            req.commit_sha.as_deref(),
            None,
            &detail,
        )
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("record evidence failed: {e}"),
            )
        })?;
        evidence_ids.push(eid);
    }

    // Shared gatekeeper auto-verify (the ONLY →verified path). Fires only on a
    // passing verdict and only when every required criterion now passes.
    let verified = req.verdict == "pass" && try_auto_verify(pool, item.id, &req.source).await;

    Ok(Json(serde_json::json!({
        "public_id": req.public_id,
        "source": req.source,
        "criteria_evidenced": evidence_ids.len(),
        "evidence_ids": evidence_ids,
        "verified": verified,
    })))
}

// ============================================================================
// POST /api/tracker/pr_event — a PR opened/merged advances a work item
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct TrackerPrEventRequest {
    /// Must match `[tracker] user_token` (the trusted-producer credential — a
    /// PR webhook forwarder holds it, the agent does not).
    pub token: String,
    /// The work item's `public_id`, if the forwarder knows it. Else resolved
    /// from `branch` (via an existing git-link) or by parsing the PR text.
    #[serde(default)]
    pub public_id: Option<String>,
    /// The PR's source branch (used to resolve the item from a prior link, and
    /// recorded as a `branch` git-link).
    #[serde(default)]
    pub branch: Option<String>,
    /// The PR number (recorded as a `pr` git-link).
    #[serde(default)]
    pub pr_number: Option<i64>,
    /// The webhook action (`opened` | `closed` | `merged` | …) — informational;
    /// `merged` is driven by the `merged` flag below.
    pub action: String,
    /// Whether the PR was merged (the close-the-loop trigger).
    #[serde(default)]
    pub merged: Option<bool>,
    /// The merge commit SHA (recorded as a `commit` git-link when present).
    #[serde(default)]
    pub commit_sha: Option<String>,
    /// PR title/body, parsed for `#<public_id>` / `fixes <public_id>` when no
    /// explicit `public_id`/`branch` resolves the item.
    #[serde(default)]
    pub text: Option<String>,
    /// Project name to scope branch/commit resolution (defaults to the item's).
    #[serde(default)]
    pub project: Option<String>,
}

/// React to a PR lifecycle event. Token-gated. Resolves the work item (explicit
/// `public_id`, else a prior `branch` git-link, else by parsing the PR `text`),
/// upserts `pr` / `branch` / `commit` git-links, and — ON MERGE — runs the
/// **`Actor::Agent`** advance toward `verifying` (a verify *candidate*).
///
/// TRUST BOUNDARY: a merge is an agent-grade signal. It advances at most to
/// `verifying`; it can NEVER reach `verified`, because the walk uses only
/// `Actor::Agent` steps and the matrix has no `Agent` arm into `verified`.
/// `→verified` still requires CI evidence via `/api/tracker/ci_evidence`. The
/// response says so explicitly.
pub async fn tracker_pr_event(
    State(state): State<ApiState>,
    Json(req): Json<TrackerPrEventRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let token_ok = {
        let cfg = state.config.load();
        cfg.tracker
            .user_token
            .as_deref()
            .map(|t| t == req.token)
            .unwrap_or(false)
    };
    if !token_ok {
        return Err((
            StatusCode::FORBIDDEN,
            "invalid or missing tracker token (set [tracker] user_token)".to_string(),
        ));
    }
    // Audit the trust-boundary crossing (token never logged — only that the gate
    // passed). A merge event stages a `verifying` candidate; CI evidence still
    // gates the final `verified` flip.
    tracing::info!(
        target: "pgmcp::tracker::audit",
        endpoint = "pr_event",
        public_id = ?req.public_id,
        action = %req.action,
        pr_number = ?req.pr_number,
        merged = ?req.merged,
        "tracker PR event accepted (token gate passed)"
    );
    let pool = state.db.pool().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "raw pool unavailable".to_string(),
    ))?;

    // Resolve the work item: explicit public_id → branch git-link → parse text.
    let item = if let Some(pid) = req.public_id.as_deref().filter(|s| !s.trim().is_empty()) {
        crate::db::queries::get_work_item_by_public_id(pool, pid)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")))?
    } else if let Some(branch) = req.branch.as_deref().filter(|s| !s.trim().is_empty()) {
        // An item previously linked to this branch.
        let item_id: Option<i64> = sqlx::query_scalar(
            "SELECT item_id FROM work_item_git_links \
             WHERE link_type = 'branch' AND ref_value = $1 ORDER BY id LIMIT 1",
        )
        .bind(branch)
        .fetch_optional(pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")))?;
        match item_id {
            Some(id) => crate::db::queries::get_work_item(pool, id)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")))?,
            None => None,
        }
    } else if let Some(text) = req.text.as_deref() {
        // Parse the PR text for a #<public_id> / fixes <public_id> reference.
        let ids = crate::tracker::commit_ref::extract_public_ids(text);
        match ids.first() {
            Some(pid) => crate::db::queries::get_work_item_by_public_id(pool, pid)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")))?,
            None => None,
        }
    } else {
        None
    };
    let item = item.ok_or((
        StatusCode::NOT_FOUND,
        "could not resolve a work item (provide public_id, a linked branch, or PR text with a \
         #<public_id> reference)"
            .to_string(),
    ))?;

    let scope_project_id = match req.project.as_deref() {
        Some(name) => crate::db::queries::resolve_project_id(pool, Some(name))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")))?,
        None => item.project_id,
    };

    // Upsert the PR / branch / commit git-links (idempotent).
    let mut links: Vec<serde_json::Value> = Vec::new();
    if let Some(pr) = req.pr_number {
        let (_, created) = crate::db::queries::insert_git_link(
            pool,
            item.id,
            scope_project_id,
            crate::tracker::git_link::GitLinkType::Pr.as_str(),
            &pr.to_string(),
            None,
            "auto_scan",
            None,
        )
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")))?;
        links.push(serde_json::json!({"type": "pr", "ref": pr, "created": created}));
    }
    if let Some(branch) = req.branch.as_deref().filter(|s| !s.trim().is_empty()) {
        let (_, created) = crate::db::queries::insert_git_link(
            pool,
            item.id,
            scope_project_id,
            crate::tracker::git_link::GitLinkType::Branch.as_str(),
            branch,
            None,
            "auto_scan",
            None,
        )
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")))?;
        links.push(serde_json::json!({"type": "branch", "ref": branch, "created": created}));
    }
    if let Some(sha) = req.commit_sha.as_deref().filter(|s| !s.trim().is_empty()) {
        let commit_id = match scope_project_id {
            Some(pid) => crate::db::queries::resolve_commit_id(pool, pid, sha)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")))?,
            None => None,
        };
        let (_, created) = crate::db::queries::insert_git_link(
            pool,
            item.id,
            scope_project_id,
            crate::tracker::git_link::GitLinkType::Commit.as_str(),
            sha,
            commit_id,
            "auto_scan",
            None,
        )
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")))?;
        links.push(serde_json::json!({"type": "commit", "ref": sha, "created": created}));
    }

    // On merge, advance toward `verifying` as Actor::Agent — NEVER `verified`.
    let mut advanced_to: Option<String> = None;
    let is_merged = req.merged.unwrap_or(false) || req.action.eq_ignore_ascii_case("merged");
    if is_merged {
        advanced_to = advance_agent_to_verifying(pool, item.id).await;
    }

    Ok(Json(serde_json::json!({
        "public_id": item.public_id,
        "action": req.action,
        "merged": is_merged,
        "links": links,
        "advanced_to": advanced_to,
        "note": "A merge is an agent-grade signal: it advances at most to 'verifying' (a verify \
                 candidate) and can NEVER reach 'verified'. Post CI evidence to \
                 /api/tracker/ci_evidence to close the loop to verified.",
    })))
}

/// Walk an item to `verifying` using ONLY `Actor::Agent` steps legal from its
/// current state. Returns the status it ended at (or `None` if no advance was
/// possible / it was already past `verifying`). Every step runs through
/// `set_work_item_status` → `check_transition`, so this cannot bypass the
/// chokepoint; and because no step is `Actor::Gatekeeper`, it can never reach
/// `verified`/`rejected`. Best-effort: a refused step halts the walk.
async fn advance_agent_to_verifying(pool: &sqlx::PgPool, item_id: i64) -> Option<String> {
    use crate::tracker::status::WorkItemStatus::{
        Blocked, ClaimedDone, Confirmed, InProgress, Pending, Ready, Verifying,
    };
    use crate::tracker::transition::Actor;
    let row = crate::db::queries::get_work_item(pool, item_id)
        .await
        .ok()??;
    let from = crate::tracker::status::WorkItemStatus::parse(&row.status)?;
    // Agent-legal steps to reach `verifying` from each startable/in-flight state.
    // (Pending/Confirmed/Ready/Blocked → in_progress → verifying;
    //  in_progress/claimed_done → verifying directly.)
    let steps: &[WorkItemStatusTarget] = match from {
        Pending | Confirmed | Ready | Blocked => &[
            WorkItemStatusTarget(InProgress),
            WorkItemStatusTarget(Verifying),
        ],
        InProgress | ClaimedDone => &[WorkItemStatusTarget(Verifying)],
        // verifying/verified/rejected/deferred/cancelled/triage: nothing to do
        // (triage's only exit is the user-only → confirmed).
        _ => &[],
    };
    let mut ended: Option<String> = None;
    for WorkItemStatusTarget(to) in steps.iter().copied() {
        match crate::db::queries::set_work_item_status(
            pool,
            item_id,
            to,
            Actor::Agent,
            Some("pr-webhook"),
            Some("git: PR merged (verify candidate)"),
            None,
            None,
        )
        .await
        {
            Ok(_) => ended = Some(to.as_str().to_string()),
            Err(_) => break, // refused (e.g. concurrent change) — stop the walk
        }
    }
    ended
}

/// Newtype so the `steps` slices above are `'static` (a bare
/// `&[WorkItemStatus]` literal is fine, but wrapping keeps the match arms
/// uniform and `Copy`-friendly for the `.iter().copied()` walk).
#[derive(Clone, Copy)]
struct WorkItemStatusTarget(crate::tracker::status::WorkItemStatus);

// ============================================================================
// GET /api/mandates?project=name&cwd=/path — Effective mandates
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct MandatesQuery {
    pub project: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MandatesResponse {
    pub requested_project: Option<String>,
    pub requested_cwd: Option<String>,
    pub found_project: bool,
    pub mandates: crate::mandates::MandateBundle,
}

pub async fn mandates(
    State(state): State<ApiState>,
    Query(params): Query<MandatesQuery>,
) -> Result<Json<MandatesResponse>, (StatusCode, String)> {
    let project = crate::mandates::resolve_project_for_mandates(
        state.db.as_ref(),
        params.project.as_deref(),
        params.cwd.as_deref(),
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Project lookup failed: {}", e),
        )
    })?;

    let config = state.config.load();
    let bundle = crate::mandates::resolve_effective_mandates(&config, project.as_ref());

    Ok(Json(MandatesResponse {
        requested_project: params.project,
        requested_cwd: params.cwd,
        found_project: project.is_some(),
        mandates: bundle,
    }))
}

// ============================================================================
// GET /api/status — Daemon health & model-state snapshot
// ============================================================================

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    /// Daemon-side runtime fields. None of these are persisted; they
    /// only make sense while the daemon is running.
    pub daemon: DaemonInfo,
    /// Connection details with the password redacted.
    pub database: DatabaseInfo,
    /// Embedding model info (model name, dim, pool size, backend, device).
    pub embeddings: EmbeddingsInfo,
    /// Per-pool capacity for the three-pool architecture
    /// (InferencePool / CronPool / GeneralPool).
    pub pools: PoolsInfo,
    /// Cron-job tunables that affect cross-project similarity output.
    pub similarity_config: SimilarityConfigInfo,
    /// Per-table counts + freshness timestamps from `status_snapshot`.
    pub model_state: StatusSnapshot,
    /// Live in-process counters from `StatsTracker`.
    pub counters: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct DaemonInfo {
    pub version: &'static str,
    pub uptime_secs: u64,
    pub current_rss_bytes: u64,
    pub peak_rss_bytes: u64,
    pub heavy_cron_running: bool,
    pub http_mcp_sessions: u64,
    /// MCP HTTP listener address (`mcp.host:mcp.port`).
    pub bind_addr: String,
    /// Path to the daemon log file (config.logging.file).
    pub log_path: String,
}

#[derive(Debug, Serialize)]
pub struct DatabaseInfo {
    pub url: String,
    pub host: String,
    pub port: u16,
    pub name: String,
    pub max_connections: u32,
    pub pool_size: u32,
    pub pool_idle: usize,
    /// `pool_size - pool_idle` — connections currently checked out.
    pub pool_active: u32,
    pub server_version: Option<String>,
    pub vector_extension_version: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EmbeddingsInfo {
    pub model: String,
    pub dimensions: usize,
    /// `embeddings.pool_size` — number of `InferencePool` workers, each
    /// owning one BertModel + tokenizer + Device. Surface as
    /// "InferencePool workers" in the CLI rendering.
    pub pool_size: usize,
    /// Inference backend (always "candle" since the Step-1 migration of
    /// the candle plan; surfaced explicitly so operators don't have to
    /// `cargo tree` to find out).
    pub backend: &'static str,
    /// "cuda:0" if `use_gpu = true`, else "cpu". Reflects the
    /// configuration; if CUDA init fails at startup, the worker logs the
    /// error and exits — the daemon does not silently fall back.
    pub device: String,
    /// Tokenizer truncation cap. Inputs that tokenize to more tokens are
    /// truncated.
    pub max_length: usize,
    /// Cap on input texts per `BertModel::forward` call. The full batch
    /// is sliced into chunks of this size to keep attention memory
    /// bounded.
    pub inference_batch_size: usize,
}

/// Per-pool capacity snapshot for the three role-specialized pools.
///
/// `InferencePool` is the GPU-bound pool — workers own ONNX/candle
/// sessions and run the full file-indexing pipeline end-to-end.
/// `CronPool` is a small dedicated pool that serves cron-task bodies so
/// a heavy `block_on` job doesn't stall light cleanup tasks. `GeneralPool`
/// is the catch-all CPU-bound pool used for parallel betweenness
/// centrality and similar non-GPU non-cron work.
#[derive(Debug, Serialize)]
pub struct PoolsInfo {
    pub inference: InferencePoolInfo,
    pub cron: CronPoolInfo,
    pub general: GeneralPoolInfo,
}

#[derive(Debug, Serialize)]
pub struct InferencePoolInfo {
    /// Configured worker count (`embeddings.pool_size`).
    pub configured_workers: usize,
}

#[derive(Debug, Serialize)]
pub struct CronPoolInfo {
    /// Hardcoded; see `src/cli/daemon.rs` (currently 2).
    pub configured_workers: usize,
}

#[derive(Debug, Serialize)]
pub struct GeneralPoolInfo {
    pub min_threads: usize,
    pub max_threads: usize,
    /// Live count from `stats.active_work_pool_threads` — the GeneralPool
    /// scaling monitor parks/unparks workers as RSS pressure rises and
    /// falls.
    pub active_workers: u64,
    /// Live count from `stats.work_pool_queue_depth`.
    pub queue_depth: u64,
}

#[derive(Debug, Serialize)]
pub struct SimilarityConfigInfo {
    pub threshold: f64,
    pub top_k: i32,
}

pub async fn status(
    State(state): State<ApiState>,
) -> Result<Json<StatusResponse>, (StatusCode, String)> {
    let pool = state.db.pool().ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "status endpoint requires a real PgPool DbClient (mock unsupported)".to_string(),
        )
    })?;

    let snapshot = status_snapshot(pool).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("status_snapshot failed: {}", e),
        )
    })?;

    let cfg = state.config.load();
    let db_cfg = &cfg.database;

    let daemon = DaemonInfo {
        version: env!("CARGO_PKG_VERSION"),
        uptime_secs: state.stats.uptime_start.elapsed().as_secs(),
        current_rss_bytes: state
            .stats
            .current_rss_bytes
            .load(std::sync::atomic::Ordering::Acquire),
        peak_rss_bytes: state
            .stats
            .peak_rss_bytes
            .load(std::sync::atomic::Ordering::Acquire),
        heavy_cron_running: state
            .stats
            .heavy_cron_running
            .load(std::sync::atomic::Ordering::Acquire),
        http_mcp_sessions: state
            .stats
            .http_mcp_sessions
            .load(std::sync::atomic::Ordering::Acquire),
        bind_addr: format!("{}:{}", cfg.mcp.host, cfg.mcp.port),
        log_path: cfg.logging.file.clone(),
    };

    let pool_size = pool.size();
    let pool_idle = pool.num_idle();
    let pool_active = pool_size.saturating_sub(pool_idle as u32);

    let database = DatabaseInfo {
        url: db_cfg.connection_url_redacted(),
        host: db_cfg.host.clone(),
        port: db_cfg.port,
        name: db_cfg.name.clone(),
        max_connections: db_cfg.max_connections,
        pool_size,
        pool_idle,
        pool_active,
        server_version: snapshot.server_version.clone(),
        vector_extension_version: snapshot.vector_extension_version.clone(),
    };

    let device = if cfg.embeddings.use_gpu {
        "cuda:0".to_string()
    } else {
        "cpu".to_string()
    };
    let embeddings = EmbeddingsInfo {
        model: cfg.embeddings.model.clone(),
        dimensions: cfg.embeddings.dimensions,
        pool_size: cfg.embeddings.pool_size,
        backend: "candle",
        device,
        max_length: cfg.embeddings.max_length,
        inference_batch_size: cfg.embeddings.inference_batch_size,
    };

    let pools = PoolsInfo {
        inference: InferencePoolInfo {
            configured_workers: cfg.embeddings.pool_size,
        },
        cron: CronPoolInfo {
            // Mirrors the hardcoded `cron_pool` in `src/cli/daemon.rs`.
            configured_workers: 2,
        },
        general: GeneralPoolInfo {
            min_threads: cfg.work_pool.min_threads,
            max_threads: cfg.work_pool.resolved_max_threads(),
            active_workers: state
                .stats
                .active_work_pool_threads
                .load(std::sync::atomic::Ordering::Acquire),
            queue_depth: state
                .stats
                .work_pool_queue_depth
                .load(std::sync::atomic::Ordering::Acquire),
        },
    };

    let similarity_config = SimilarityConfigInfo {
        threshold: cfg.cron.similarity_threshold,
        top_k: cfg.cron.similarity_top_k,
    };

    Ok(Json(StatusResponse {
        daemon,
        database,
        embeddings,
        pools,
        similarity_config,
        model_state: snapshot,
        counters: state.stats.snapshot(),
    }))
}
