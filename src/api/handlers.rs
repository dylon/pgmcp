//! REST API handlers for the pgmcp daemon.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use super::ApiState;
use crate::db::queries::{
    AcceptanceCriterionRow, BugDetailsRow, StatusSnapshot, TimelineRow, WorkItemFilter,
    WorkItemRow, current_realtime_seq, fetch_bug_details, get_work_item_by_public_id,
    get_work_item_subtree, list_acceptance_criteria, list_work_items, next_actionable_work_items,
    resolve_project_id, status_snapshot, work_item_timeline,
};

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
    // Live DB readiness from the `crate::health` breaker (a pure atomic read —
    // still no DB query on this hot path). The pool object outlives an outage,
    // so the old `pool().is_some()` stayed `true` for the entire 2026-06-11
    // downtime; the breaker reflects the *live* state. Require both: a pool must
    // exist (false in CLI mode) AND the breaker must report up.
    let db_snap = state.stats.db_health().snapshot();
    let db_ready = db_snap.up && state.db.pool().is_some();
    let embedder_ready = state.query_embedder.is_ready();
    let serving_ready = db_ready && embedder_ready;
    let mut payload = serde_json::json!({
        "phase": state.lifecycle.current().label(),
        "serving_ready": serving_ready,
        "db_ready": db_ready,
        "embedder_ready": embedder_ready,
        "ready_workers": state.query_embedder.ready_workers(),
    });
    if !db_snap.up {
        payload["db_down_since"] = serde_json::json!(db_snap.down_since_epoch);
    }
    let body = Json(payload);
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
    // whose contract does not carry a dedupe flag. Keep this endpoint's default
    // stable; callers that need deduplication use the richer search surfaces.
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
// POST /api/query — Closed read/query surface for the web UI
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct QueryRequest {
    pub mode: String,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub glob: Option<String>,
    #[serde(default)]
    pub limit: Option<i32>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct QueryResponse {
    pub mode: String,
    pub data: serde_json::Value,
}

pub async fn query(
    State(state): State<ApiState>,
    Json(req): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, (StatusCode, String)> {
    let mode = req.mode.trim().to_ascii_lowercase();
    let limit = req.limit.unwrap_or(10).clamp(1, 100);
    match mode.as_str() {
        "semantic" | "search" => {
            let query = req.query.ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    "query mode requires `query`".to_string(),
                )
            })?;
            let Json(response) = search(
                State(state.clone()),
                Json(SearchRequest {
                    query,
                    limit: Some(limit),
                    project: req.project,
                    language: req.language,
                }),
            )
            .await?;
            Ok(Json(QueryResponse {
                mode,
                data: serde_json::to_value(response).map_err(json_error)?,
            }))
        }
        "text" | "fts" => {
            let query = req.query.ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    "text mode requires `query`".to_string(),
                )
            })?;
            let results = state
                .db
                .text_search(
                    &query,
                    limit,
                    req.language.as_deref(),
                    req.project.as_deref(),
                    false,
                )
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("text_search failed: {}", e),
                    )
                })?;
            Ok(Json(QueryResponse {
                mode,
                data: serde_json::json!({
                    "results": results,
                    "truncated": results.len() == limit as usize,
                }),
            }))
        }
        "grep" | "regex" => {
            let pattern = req.pattern.or(req.query).ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    "grep mode requires `pattern` or `query`".to_string(),
                )
            })?;
            let results = state
                .db
                .grep_search_chunks(
                    &pattern,
                    req.project.as_deref(),
                    req.language.as_deref(),
                    req.glob.as_deref(),
                    false,
                    limit,
                    false,
                )
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("grep_search_chunks failed: {}", e),
                    )
                })?;
            Ok(Json(QueryResponse {
                mode,
                data: serde_json::json!({
                    "results": results,
                    "truncated": results.len() == limit as usize,
                }),
            }))
        }
        _ => Err((
            StatusCode::BAD_REQUEST,
            "mode must be one of semantic, search, text, fts, grep, regex".to_string(),
        )),
    }
}

// ============================================================================
// POST /api/file_envelope — File metadata for the read-context hook
// ============================================================================

/// Compact envelope returned to `~/.claude/hooks/pgmcp-read-context.sh`
/// when the model is about to `Read` a file: language, line count,
/// last_indexed_at. The response is intentionally the compact metadata already
/// exposed by `file_info`, keeping the read-context hook deterministic and cheap.
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relative_path: Option<String>,
    pub chunk: String,
    pub similarity: f64,
    pub language: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<i32>,
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
            Err(e) => tracing::error!(error = %e, "ColBERT rerank failed; using RRF order"),
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
            Ok(Err(e)) => tracing::error!(error = %e, "hook rerank failed; using RRF order"),
            Err(e) => {
                tracing::error!(error = %e, "hook rerank task join failed; using RRF order")
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
                relative_path: Some(r.relative_path.clone()),
                chunk: r.chunk_content.clone(),
                similarity: score,
                language: r.language.clone(),
                project_name: Some(r.project_name.clone()),
                start_line: Some(r.start_line),
                end_line: Some(r.end_line),
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
// POST /api/client/file_event — record a client file-touch (Phase 2A hook)
// ============================================================================

#[derive(Debug, Serialize, Deserialize)]
pub struct ClientFileEventRequest {
    /// Agent session UUID (the Claude hook sends one). Optional — Codex and other
    /// agents whose session id is not a UUID simply omit it; attribution then
    /// rests on `agent_id` + the resolved project. A non-UUID string still 400s
    /// (it cannot deserialize to `Uuid`), so such producers must omit the key.
    #[serde(default)]
    pub session_id: Option<uuid::Uuid>,
    pub cwd: String,
    pub file_path: String,
    /// Closed `FileOp` vocab: open|read|write|edit|close. Unknown values are
    /// rejected (400) so a typo can't land an unconstrained row.
    pub op: String,
    /// Which agent produced the touch: `claude-code` | `codex` | … . The Claude
    /// hook sends `"claude-code"`; the Codex hook (ADR-022) sends `"codex"`.
    /// Optional for backward compatibility — absent ⇒ `claude-code`. Recorded in
    /// `client_file_events.agent_id` (orthogonal to `source`, which stays the
    /// *mechanism* `client_hook`), so `client_project_matrix` can tell Codex hook
    /// rows from Claude ones — the old `client_hook ⇒ claude-code` assumption no
    /// longer holds now that two agents share the hook ingest.
    #[serde(default)]
    pub agent_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ClientFileEventResponse {
    pub recorded: bool,
    pub project_id: Option<i32>,
    pub file_id: Option<i64>,
}

/// Records one client↔file event from the Claude Code `PostToolUse` hook
/// (`~/.claude/hooks/pgmcp-file-event.sh`). Resolves the project (longest-prefix
/// cwd) and the indexed file (by absolute path), validates `op` against the
/// closed `FileOp` vocabulary, and inserts into `client_file_events` with
/// `source='client_hook'`. Hook-side identity is the Claude `session_id`; the
/// MCP `mcp_session_id` / PID are left NULL (PID-native sources fill those).
pub async fn client_file_event(
    State(state): State<ApiState>,
    Json(req): Json<ClientFileEventRequest>,
) -> Result<Json<ClientFileEventResponse>, (StatusCode, String)> {
    use crate::proc_clients::file_events::{FileEventSource, FileTouchEvent};

    // Honor the `[clients] file_events` switch — a no-op (not an error) when
    // off, so the hook stays harmless even if left wired in settings.json.
    if !state.config.load().clients.file_events {
        return Ok(Json(ClientFileEventResponse {
            recorded: false,
            project_id: None,
            file_id: None,
        }));
    }

    // Validate `op` against the closed vocabulary before touching the DB.
    let op = crate::proc_clients::file_events::FileOp::parse(&req.op).ok_or((
        StatusCode::BAD_REQUEST,
        format!(
            "invalid op '{}': expected one of open|read|write|edit|close",
            req.op
        ),
    ))?;

    // DB-availability breaker (src/health): while the DB is down, spool the raw
    // request to the outbox (replayed on recovery via loopback re-POST) instead
    // of stalling the PostToolUse hook. This is the hook path's durability story;
    // PID-native sources just drop their batches under an outage.
    if !state.stats.db_health().is_up() {
        if let Some(ob) = state.outbox.as_ref() {
            ob.append(
                "/api/client/file_event",
                serde_json::to_value(&req).unwrap_or_default(),
            );
        }
        return Ok(Json(ClientFileEventResponse {
            recorded: false,
            project_id: None,
            file_id: None,
        }));
    }

    // DB up: emit into the reactive ingestion stream (ADR-022) as a thin
    // producer. The batched writer resolves project + indexed-file once per
    // distinct path and performs the multi-row INSERT after the configured
    // `ebpf_batch_ms` window. The fire-and-forget hook does not wait on it.
    // Identity is the Claude/Codex session UUID; `agent_id` distinguishes the
    // agent behind `source='client_hook'`. A NULL project/file is fine downstream
    // (unindexed/just-written file).
    let recorded = state.stats.emit_file_event(FileTouchEvent {
        source: FileEventSource::ClientHook,
        op,
        abs_path: req.file_path,
        pid: None,
        ppid: None,
        root_pid: None,
        cgroup_id: None,
        mcp_session_id: None,
        session_id: req.session_id,
        agent_id: Some(req.agent_id.unwrap_or_else(|| "claude-code".to_string())),
    });

    Ok(Json(ClientFileEventResponse {
        recorded,
        project_id: None,
        file_id: None,
    }))
}

// ============================================================================
// GET /api/client/file_events/stream — live SSE feed of file touches (ADR-022)
// ============================================================================

/// Server-Sent Events feed of `client_file_events` rows as they land — the HTTP
/// face of the live fan-out (ADR-022). Polls by `id` cursor every 200 ms
/// (mirroring the a2a SSE bridge, so no `LISTEN` connection is held open),
/// starting at the current max id so only NEW rows stream. Emits nothing when
/// `[clients] file_event_stream` is off; external tools / pi may instead `LISTEN`
/// on the `pg_notify` channel directly.
pub async fn client_file_events_stream(
    State(state): State<ApiState>,
) -> axum::response::sse::Sse<
    impl futures::Stream<Item = Result<axum::response::sse::Event, axum::Error>>,
> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use std::time::{Duration, Instant};

    let enabled = state.config.load().clients.file_event_stream;
    let pool = state.db.pool().cloned();
    // Start at the current max id so only rows that land AFTER subscription
    // stream (a fresh subscriber doesn't replay history).
    let start_id: i64 = match (enabled, &pool) {
        (true, Some(p)) => {
            sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM client_file_events")
                .fetch_one(p)
                .await
                .unwrap_or(0)
        }
        _ => i64::MAX, // disabled / no pool → cursor past everything → emits nothing
    };

    let stream = futures::stream::unfold(
        (pool, start_id, Instant::now()),
        move |(pool, last_id, started)| async move {
            if !enabled || pool.is_none() || started.elapsed() > Duration::from_secs(300) {
                return None; // disabled, CLI-mode, or 5-min cap reached
            }
            let p = pool.as_ref().expect("pool present when enabled");
            let rows = sqlx::query_as::<_, (i64, String, String, String, Option<String>)>(
                "SELECT id, abs_path, op, source, agent_id FROM client_file_events
                 WHERE id > $1 ORDER BY id LIMIT 500",
            )
            .bind(last_id)
            .fetch_all(p)
            .await
            .unwrap_or_default();
            if rows.is_empty() {
                tokio::time::sleep(Duration::from_millis(200)).await;
                return Some((
                    Ok(Event::default().comment("heartbeat")),
                    (pool, last_id, started),
                ));
            }
            let next = rows.last().expect("non-empty").0;
            let payload = serde_json::json!({
                "events": rows.iter().map(|(id, path, op, source, agent)| serde_json::json!({
                    "id": id, "abs_path": path, "op": op, "source": source, "agent_id": agent,
                })).collect::<Vec<_>>(),
            });
            Some((
                Ok(Event::default()
                    .event("file_events")
                    .data(payload.to_string())),
                (pool, next, started),
            ))
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ============================================================================
// POST /api/client/inbox_peek — mid-loop A2A message delivery (PostToolUse hook)
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct InboxPeekRequest {
    pub session_id: uuid::Uuid,
    pub cwd: String,
    #[serde(default)]
    pub agent_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct InboxPeekResponse {
    /// Rendered "📨 Agent messages" block, or empty when nothing is pending.
    pub additional_context: String,
}

/// Returns any undelivered A2A messages for this session/project as a markdown
/// block (and marks them delivered on the `posttooluse` channel). Backs the
/// `~/.claude/hooks/pgmcp-inbox.sh` PostToolUse hook, which emits the block as
/// `additionalContext` so a mid-agentic-loop agent sees mail between tool calls.
pub async fn client_inbox_peek(
    State(state): State<ApiState>,
    Json(req): Json<InboxPeekRequest>,
) -> Result<Json<InboxPeekResponse>, (StatusCode, String)> {
    let pool = state.db.pool().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "raw pool unavailable".to_string(),
    ))?;
    let project_id = state
        .db
        .find_project_by_cwd(&req.cwd)
        .await
        .ok()
        .flatten()
        .map(|p| p.id);
    let recipient_session = req.session_id.to_string();
    let block = crate::a2a::delivery::render_and_deliver(
        pool,
        Some(&recipient_session),
        project_id,
        req.agent_id.as_deref(),
        crate::a2a::mailbox::DeliveryChannel::Posttooluse.as_str(),
        5,
    )
    .await
    .unwrap_or_default();
    Ok(Json(InboxPeekResponse {
        additional_context: block,
    }))
}

// ============================================================================
// POST /api/tracker/project_event — git-state gatekeeper (resolves coordination)
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct ProjectEventRequest {
    pub project: String,
    pub kind: String,
}

#[derive(Debug, Serialize)]
pub struct ProjectEventResponse {
    pub recorded: bool,
    pub resolved_requests: Vec<i64>,
}

/// The coordination gatekeeper seam (parallels `ci_evidence`/`pr_event`): an
/// external git scanner or CI posts a project git-state event. A
/// `stable_restored` event for a dependency resolves the open coordination
/// requests against it and notifies the unblocked requesters — the only
/// non-cron path to `resolved`, preserving the trust boundary proven in
/// `docs/formal/WorktreeNegotiation.{tla,v}`.
pub async fn tracker_project_event(
    State(state): State<ApiState>,
    Json(req): Json<ProjectEventRequest>,
) -> Result<Json<ProjectEventResponse>, (StatusCode, String)> {
    let pool = state.db.pool().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "raw pool unavailable".to_string(),
    ))?;
    let kind = crate::deps::coordination::ProjectEventKind::parse(&req.kind).ok_or((
        StatusCode::BAD_REQUEST,
        format!(
            "invalid kind '{}': expected stable_restored | went_unstable",
            req.kind
        ),
    ))?;
    let pid: Option<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
        .bind(&req.project)
        .fetch_optional(pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let Some(pid) = pid else {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("unknown project '{}'", req.project),
        ));
    };
    sqlx::query("INSERT INTO project_events (project_id, kind) VALUES ($1, $2)")
        .bind(pid)
        .bind(kind.as_str())
        .execute(pool)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("event insert: {e}"),
            )
        })?;
    let resolved_requests = if kind == crate::deps::coordination::ProjectEventKind::StableRestored {
        crate::deps::coord_store::resolve_and_notify(pool, pid)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("resolve: {e}")))?
    } else {
        Vec::new()
    };
    Ok(Json(ProjectEventResponse {
        recorded: true,
        resolved_requests,
    }))
}

// ============================================================================
// POST /api/session/observe — Session-mandate observation + re-injection
// ============================================================================

#[derive(Debug, Serialize, Deserialize)]
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
    // DB-availability breaker (src/health): if the database is unreachable,
    // spool the raw request to the outbox (replayed on recovery via re-POST to
    // this same endpoint) and return a neutral response, rather than stalling on
    // the pool acquire and failing the UserPromptSubmit hook. The prompt is the
    // highest-value ephemeral datum (it drives cross-session retrieval + mandate
    // re-injection) and has no other durable source.
    if !state.stats.db_health().is_up() {
        if let Some(ob) = state.outbox.as_ref() {
            ob.append(
                "/api/session/observe",
                serde_json::to_value(&req).unwrap_or_default(),
            );
        }
        return Ok(Json(ObserveResponse {
            session_id: req.session_id,
            prompt_id: -1,
            extracted: Vec::new(),
            active_mandates: Vec::new(),
            rag_hits: Vec::new(),
            additional_context: String::new(),
        }));
    }

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

    // Make the work-item presence layer project-aware: record this agent's
    // session + current project so the active-agents-by-project view can join
    // agent → project. Fire-and-forget; never blocks the observe response.
    if let Some(agent_id) = req.agent_id.as_deref() {
        let _ = crate::db::queries::touch_agent_presence_project(
            pool,
            agent_id,
            req.session_id,
            project_id,
        )
        .await;
    }

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
                        tracing::error!(error = %e, "mark_near_duplicate_superseded failed")
                    }
                }
            }
            Err(e) => tracing::error!(error = %e, "upsert_mandate failed"),
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
                    relative_path: Some(r.relative_path),
                    chunk: r.chunk_content,
                    similarity: r.score.unwrap_or(0.0),
                    language: r.language,
                    project_name: Some(r.project_name),
                    start_line: Some(r.start_line),
                    end_line: Some(r.end_line),
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

    // 📨 Agent mailbox: surface undelivered messages for this session/project on
    // the model-visible next-turn channel (UserPromptSubmit). Receipt-deduped per
    // session so each message appears once; budget-shared with the 2 KB block.
    // (Session-addressed messages — keyed by mcp_session_id — arrive via the
    // `a2a_inbox` pull instead; here we deliver project- and agent-broadcasts.)
    if additional_context.len() < 1900
        && let Some(block) = crate::a2a::delivery::render_and_deliver(
            pool,
            Some(&req.session_id.to_string()),
            project_id,
            req.agent_id.as_deref(),
            crate::a2a::mailbox::DeliveryChannel::Prompt.as_str(),
            5,
        )
        .await
        && additional_context.len() + block.len() + 1 < 2048
    {
        if !additional_context.is_empty() {
            additional_context.push('\n');
        }
        additional_context.push_str(&block);
    }

    // Phase 4 (ADR-009 §4.6): proactive dependency-edit warnings. Surface
    // "a dependency you rely on is being edited (dirty) by <agent>" for the
    // dependencies of this project that are dirty, have a live editor, and are not
    // already under an open coordination request from here (that open-request
    // check is the dedup — once you `coordinate_dependency_block`, it goes quiet).
    // Off unless [a2a] proactive_dependency_warnings = true. Read-only;
    // budget-shared with the 2 KB block.
    if state.config.load().a2a.proactive_dependency_warnings
        && additional_context.len() < 1900
        && let Some(pid) = project_id
        && let Ok(warns) = crate::deps::coord_store::pending_dependency_warnings(pool, pid, 3).await
        && !warns.is_empty()
    {
        let mut block = String::from("\n## ⚠ Dependencies being edited (pgmcp)\n");
        for w in &warns {
            block.push_str(&format!(
                "- **{}** is being edited (dirty) by {} — your build may break; \
                 `coordinate_dependency_block{{dependency:\"{}\"}}` to request a worktree move.\n",
                w.dependency_name,
                w.editors.as_deref().unwrap_or("an agent"),
                w.dependency_name,
            ));
        }
        if additional_context.len() + block.len() < 2048 {
            additional_context.push_str(&block);
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

// ============================================================================
// POST /api/scanner/findings — ingest pi-run linter (or scanner) diagnostics
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct ScannerFindingIngest {
    #[serde(default)]
    pub rule_id: Option<String>,
    /// critical | high | medium | low (other strings map to low; tool levels like
    /// "error"/"warning" are accepted and mapped).
    pub severity: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<i32>,
    pub title: String,
    #[serde(default)]
    pub message: Option<String>,
    /// Tool-native finding JSON (default `{}`).
    #[serde(default)]
    pub raw: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct ScannerFindingsRequest {
    /// Must match `[tracker] user_token` — the trusted-producer credential.
    pub token: String,
    /// Project name (resolved to `project_id`).
    pub project: String,
    /// Tool slug, e.g. "clippy" | "eslint" | "clj-kondo" | "tsc".
    pub scanner: String,
    /// Finding class: "lint" (default) | "security".
    #[serde(default)]
    pub finding_class: Option<String>,
    pub findings: Vec<ScannerFindingIngest>,
}

/// Ingest externally-run scanner/linter findings (ADR-014 E7). pi runs the linter
/// (the no-file boundary keeps pgmcp from spawning it) and POSTs the parsed
/// diagnostics here; pgmcp persists them in `external_scanner_findings` (idempotent
/// on a derived fingerprint), tagging `finding_class='lint'` by default so they are
/// queryable/trended without masquerading as vulnerabilities. Credential-gated by
/// `[tracker] user_token` (a trusted-producer route, like `/api/tracker/ci_evidence`);
/// it stores findings only — no work-item transitions.
pub async fn scanner_findings_ingest(
    State(state): State<ApiState>,
    Json(req): Json<ScannerFindingsRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    use sha2::{Digest, Sha256};

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
    let finding_class = match req.finding_class.as_deref().unwrap_or("lint") {
        c @ ("lint" | "security") => c,
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                "finding_class must be \"lint\" or \"security\"".to_string(),
            ));
        }
    };
    let pool = state.db.pool().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "raw pool unavailable".to_string(),
    ))?;
    let project_id = crate::db::queries::resolve_project_id(pool, Some(&req.project))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")))?
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("no indexed project '{}'", req.project),
        ))?;

    let run_id = crate::db::queries::insert_scanner_run(
        pool,
        project_id,
        &req.scanner,
        "ok",
        None,
        0,
        req.findings.len() as i32,
        None,
        Some("ingested via POST /api/scanner/findings"),
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("insert run: {e}"),
        )
    })?;

    let mut seen: Vec<String> = Vec::with_capacity(req.findings.len());
    let mut stored = 0u64;
    for f in &req.findings {
        let severity = match f.severity.to_ascii_lowercase().as_str() {
            "critical" => "critical",
            "high" | "error" => "high",
            "medium" | "warning" | "warn" => "medium",
            _ => "low",
        };
        let file = f.file.as_deref().unwrap_or("");
        let rule = f.rule_id.as_deref().unwrap_or("");
        let mut hasher = Sha256::new();
        hasher.update(
            format!(
                "{}|{}|{}|{}|{}|{}",
                req.scanner,
                req.project,
                file,
                f.line.unwrap_or(0),
                rule,
                f.title
            )
            .as_bytes(),
        );
        let fingerprint = format!("{:x}", hasher.finalize());
        let provenance_key = format!("{}:{}", req.scanner, fingerprint);
        let raw = f.raw.clone().unwrap_or_else(|| serde_json::json!({}));
        match crate::db::queries::upsert_scanner_finding(
            pool,
            project_id,
            run_id,
            &req.scanner,
            f.rule_id.as_deref(),
            severity,
            f.file.as_deref(),
            f.line,
            &f.title,
            f.message.as_deref(),
            &raw,
            &fingerprint,
            &provenance_key,
            finding_class,
        )
        .await
        {
            Ok(()) => {
                stored += 1;
                seen.push(fingerprint);
            }
            Err(e) => {
                tracing::error!(scanner = %req.scanner, error = %e, "scanner-ingest: upsert finding failed");
            }
        }
    }
    // A previously-open finding for this (project, scanner) not in this batch is
    // resolved (the linter ran and no longer reports it). Scanner slugs are
    // class-disjoint (clippy/eslint vs gitleaks/semgrep), so this never crosses
    // the lint/security boundary.
    let resolved = crate::db::queries::mark_unseen_resolved(pool, project_id, &req.scanner, &seen)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("resolve: {e}")))?;

    // Realtime event (topic=scanner): a findings batch landed. Own-tx,
    // best-effort — the ingest response must not fail on a telemetry write.
    crate::realtime::emit(
        pool,
        &crate::realtime::RealtimeEvent::scanner_append(&req.project, &req.scanner, stored, run_id),
    )
    .await;

    Ok(Json(serde_json::json!({
        "ok": true,
        "project_id": project_id,
        "scanner": req.scanner,
        "finding_class": finding_class,
        "run_id": run_id,
        "stored": stored,
        "resolved": resolved,
    })))
}

// ============================================================================
// POST /api/control/{halt,resume} — fleet-wide ALL-STOP (ADR-016 E8)
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct ControlRequest {
    /// Must match `[tracker] user_token` — the operator credential.
    pub token: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Credential gate shared by the control endpoints: the request `token` must
/// equal `[tracker] user_token` (an agent does not have it).
fn require_tracker_token(state: &ApiState, token: &str) -> Result<(), (StatusCode, String)> {
    let ok = {
        let cfg = state.config.load();
        cfg.tracker
            .user_token
            .as_deref()
            .map(|t| t == token)
            .unwrap_or(false)
    };
    if ok {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            "invalid or missing tracker token (set [tracker] user_token)".to_string(),
        ))
    }
}

async fn set_halted(
    state: &ApiState,
    halt: bool,
    reason: Option<&str>,
) -> Result<(), (StatusCode, String)> {
    state
        .halted
        .store(halt, std::sync::atomic::Ordering::Relaxed);
    if let Some(pool) = state.db.pool() {
        sqlx::query(
            "UPDATE system_control
                SET halted = $1,
                    halted_at = CASE WHEN $1 THEN now() ELSE halted_at END,
                    reason = $2
              WHERE id = 1",
        )
        .bind(halt)
        .bind(reason)
        .execute(pool)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("persist halt: {e}"),
            )
        })?;

        // Append the action to the append-only control-plane audit journal so the
        // halt/resume history is post-mortemable (ADR-020/D4) — the mutable
        // `system_control` row above is only the current-state cache. Best-effort:
        // a journal failure must NEVER block the stop itself, so log and continue.
        // This is the single choke point every channel funnels through (REST,
        // `scripts/all-stop.sh`, the UPS `on-power-fail.sh` hook), so every
        // fleet-wide halt/resume is journaled here exactly once.
        let action = if halt {
            crate::csm::trace_store::ControlAction::Halt
        } else {
            crate::csm::trace_store::ControlAction::Resume
        };
        let entry = crate::csm::trace_store::ControlInput {
            action,
            scope: crate::csm::trace_store::ControlScope::Fleet,
            session_key: None,
            task_id: None,
            work_item_public_id: None,
            trace_id: None,
            span_id: None,
            reason: reason.map(str::to_string),
            actor: Some("rest".to_string()),
            attributes: serde_json::json!({}),
        };
        if let Err(e) = crate::csm::trace_store::record_control(pool, &entry).await {
            tracing::error!(error = %e, "control-journal append failed (all-stop still applied)");
        }

        // Realtime event (topic=control): fleet-wide halt/resume. Own-tx,
        // best-effort — the all-stop is already applied above; a telemetry write
        // must never fail it. `actor="rest"` mirrors the control-journal entry.
        crate::realtime::emit(
            pool,
            &crate::realtime::RealtimeEvent::control(halt, reason, "rest"),
        )
        .await;
    }
    Ok(())
}

/// POST /api/control/halt — engage the fleet-wide all-stop: the A2A dispatcher
/// refuses new tasks and aborts in-flight ones at the next round boundary. The
/// flag is durable (survives a restart; resume is explicit).
pub async fn control_halt(
    State(state): State<ApiState>,
    Json(req): Json<ControlRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    require_tracker_token(&state, &req.token)?;
    set_halted(&state, true, req.reason.as_deref()).await?;
    tracing::warn!(reason = ?req.reason, "fleet ALL-STOP engaged via /api/control/halt");
    Ok(Json(serde_json::json!({ "ok": true, "halted": true })))
}

/// POST /api/control/resume — clear the all-stop; new dispatch resumes.
pub async fn control_resume(
    State(state): State<ApiState>,
    Json(req): Json<ControlRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    require_tracker_token(&state, &req.token)?;
    set_halted(&state, false, req.reason.as_deref()).await?;
    tracing::warn!("fleet all-stop cleared via /api/control/resume");
    Ok(Json(serde_json::json!({ "ok": true, "halted": false })))
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
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub as_of_seq: Option<i64>,
    /// Session UUID for the DB-backed session-mandate merge (scope ∈
    /// {all, session}). Ignored when absent or unparseable.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MandatesResponse {
    pub requested_project: Option<String>,
    pub requested_cwd: Option<String>,
    pub requested_scope: Option<String>,
    pub as_of_seq: Option<i64>,
    pub server_seq: Option<i64>,
    pub found_project: bool,
    pub mandates: crate::mandates::MandateBundle,
    /// DB-backed durable (promoted / operator-authored) mandates, retired rows
    /// excluded — populated for scope ∈ {all, global, project, workspace}. The
    /// file-backed `mandates` bundle (AGENTS.md/CLAUDE.md) never carries the
    /// promoted-rule store, so the console merges it in here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub durable_mandates: Option<Vec<crate::db::queries::DurableMandateRow>>,
    /// DB-backed active session mandates for `session_id` — populated for scope
    /// ∈ {all, session}. Fixes `scope=session` previously returning empty (no
    /// file-backed source is ever session-scoped).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_mandates: Option<Vec<crate::sessions::SessionMandate>>,
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
    let mut bundle = crate::mandates::resolve_effective_mandates(&config, project.as_ref());
    let requested_scope = params.scope.clone();
    if let Some(scope) = params.scope.as_deref() {
        filter_mandate_bundle_by_scope(&mut bundle, scope)?;
    }
    let server_seq = if let Some(pool) = state.db.pool() {
        current_realtime_seq(pool).await.ok()
    } else {
        None
    };

    // DB-backed merge (ADR-034 admin console). The `bundle` above is file-backed
    // (AGENTS.md/CLAUDE.md) only; it never carries the DB-backed durable
    // (promoted / operator-authored) or session mandates. Surface those so the
    // console's scope filter is complete — in particular `scope=session`, which
    // previously returned empty because no file source is ever session-scoped.
    // Read-only + best-effort: a query failure logs at error! (ADR-021) and
    // degrades to the file bundle alone rather than failing the whole read.
    let scope_norm = params
        .scope
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase());
    // (scope_filter, project_filter) for the durable read, or None to skip it.
    let durable_filter: Option<(Option<&str>, Option<i32>)> = match scope_norm.as_deref() {
        Some("all") => Some((None, None)),
        Some("global") => Some((Some("global"), None)),
        Some("workspace") => Some((Some("workspace"), None)),
        Some("project") => Some((Some("project"), project.as_ref().map(|p| p.id))),
        _ => None,
    };
    let mut durable_mandates = None;
    let mut session_mandates = None;
    if let Some(pool) = state.db.pool() {
        if let Some((scope_filter, project_filter)) = durable_filter {
            match crate::db::queries::list_active_durable_mandates(
                pool,
                scope_filter,
                project_filter,
            )
            .await
            {
                Ok(rows) => durable_mandates = Some(rows),
                Err(e) => {
                    tracing::error!(error = %e, "GET /api/mandates: durable mandate merge failed")
                }
            }
        }
        if matches!(scope_norm.as_deref(), Some("all") | Some("session"))
            && let Some(sid) = params
                .session_id
                .as_deref()
                .and_then(|s| uuid::Uuid::parse_str(s.trim()).ok())
        {
            match crate::sessions::list_active_mandates(pool, Some(sid), params.cwd.as_deref(), 100)
                .await
            {
                Ok(rows) => session_mandates = Some(rows),
                Err(e) => {
                    tracing::error!(error = %e, "GET /api/mandates: session mandate merge failed")
                }
            }
        }
    }

    Ok(Json(MandatesResponse {
        requested_project: params.project,
        requested_cwd: params.cwd,
        requested_scope,
        as_of_seq: params.as_of_seq,
        server_seq,
        found_project: project.is_some(),
        mandates: bundle,
        durable_mandates,
        session_mandates,
    }))
}

// ============================================================================
// GET /api/work_items?view=... — Read-only tracker smart views for the web UI
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct WorkItemsQuery {
    #[serde(default)]
    pub view: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub plan_public_id: Option<String>,
    /// Optional cross-cutting filters (layered on top of the smart-view).
    /// Validated against the closed `WorkItemKind` / `WorkItemStatus`
    /// vocabularies (unknown => 400). `project` is a project *name*, resolved
    /// to an id (unknown => 400). `parent_id` restricts to direct children of
    /// a numeric item id.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub parent_id: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct WorkItemsResponse {
    pub view: String,
    pub count: usize,
    pub server_seq: Option<i64>,
    pub items: Vec<WorkItemRow>,
}

pub async fn work_items(
    State(state): State<ApiState>,
    Query(params): Query<WorkItemsQuery>,
) -> Result<Json<WorkItemsResponse>, (StatusCode, String)> {
    let pool = real_pool(&state, "work item smart views")?;
    // Was a view explicitly requested? (`parse_work_items_view` defaults a
    // missing/blank view to `next-actionable`; we need the raw bit to decide
    // whether ad-hoc filters browse unconstrained or compose with a chosen view.)
    let view_explicit = trimmed_non_empty(params.view.as_deref()).is_some();
    let view = parse_work_items_view(params.view.as_deref())?;
    let limit = params.limit.unwrap_or(25).clamp(1, 100);
    let assignee = trimmed_non_empty(params.assignee.as_deref());
    let server_seq = current_realtime_seq(pool).await.ok();

    // Optional cross-cutting filters, validated against their closed
    // vocabularies (unknown => 400) so the filter never binds a value the DB
    // CHECK would reject. `kind`/`status` become the canonical `&'static str`
    // from the enum (parse succeeds only on an exact match).
    let kind = parse_work_item_kind_filter(params.kind.as_deref())?;
    let status_filter = parse_work_item_status_filter(params.status.as_deref())?;
    let project_id = resolve_work_items_project(pool, params.project.as_deref()).await?;
    let parent_id = params.parent_id;
    let has_extra_filter =
        kind.is_some() || status_filter.is_some() || project_id.is_some() || parent_id.is_some();

    // Plan scoping (`plan_public_id`) is only available on the dedicated
    // next-actionable path, which cannot also apply the cross-cutting filters.
    // Reject the ambiguous combinations — this preserves the pre-existing "only
    // valid for next-actionable" rule and extends it to the with-filters case.
    let plan_public_id = trimmed_non_empty(params.plan_public_id.as_deref());
    if plan_public_id.is_some()
        && (view != crate::tracker::views::SmartView::NextActionable || has_extra_filter)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "plan_public_id is only valid for the next-actionable view without \
             kind/status/project/parent_id filters"
                .to_string(),
        ));
    }

    let items = if view_explicit
        && view == crate::tracker::views::SmartView::NextActionable
        && !has_extra_filter
    {
        // Unchanged dedicated path: next-actionable, optionally plan-scoped.
        // Requires an EXPLICIT view — an absent view param falls through to the
        // unconstrained browse below (the webui "All" option omits `view`).
        let plan_root_id = match plan_public_id {
            Some(public_id) => Some(resolve_work_item_public_id(pool, public_id).await?),
            None => None,
        };
        next_actionable_work_items(pool, plan_root_id, assignee, limit)
            .await
            .map_err(work_items_query_error)?
    } else {
        // Filter path. Base = the explicitly chosen smart-view's filter, or an
        // unconstrained browse when no view was given (so an ad-hoc
        // `?status=verified` is not silently emptied by the next-actionable
        // default). Explicit params then overwrite the view-derived fields.
        let my_work_assignee = if view_explicit && view == crate::tracker::views::SmartView::MyWork
        {
            Some(assignee.unwrap_or("cli").to_string())
        } else {
            None
        };
        let mut filter = if view_explicit {
            work_items_view_filter(view, my_work_assignee.as_deref(), limit)
        } else {
            WorkItemFilter {
                limit,
                ..Default::default()
            }
        };
        // next-actionable (explicit but filter-routed) and the unconstrained
        // browse both honor an `assignee` param; my-work already encoded it
        // above; the other three views ignore it (unchanged behavior).
        if filter.assignee.is_none()
            && (!view_explicit || view == crate::tracker::views::SmartView::NextActionable)
        {
            filter.assignee = assignee;
        }
        if let Some(kind) = kind {
            filter.kind = Some(kind);
        }
        if let Some(status) = status_filter {
            filter.status = Some(status);
        }
        if let Some(project_id) = project_id {
            filter.project_id = Some(project_id);
        }
        if let Some(parent_id) = parent_id {
            filter.parent_id = Some(parent_id);
        }
        list_work_items(pool, &filter)
            .await
            .map_err(work_items_query_error)?
    };

    // Report the effective view: the chosen view, or `all` for an unconstrained
    // ad-hoc filter browse (so the pane's summary line stays honest).
    let view_label = if !view_explicit && has_extra_filter {
        "all"
    } else {
        view.as_str()
    };

    Ok(Json(WorkItemsResponse {
        view: view_label.to_string(),
        count: items.len(),
        server_seq,
        items,
    }))
}

fn parse_work_items_view(
    raw: Option<&str>,
) -> Result<crate::tracker::views::SmartView, (StatusCode, String)> {
    let raw = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(crate::tracker::views::SmartView::NextActionable.as_str());
    crate::tracker::views::SmartView::parse(raw).ok_or_else(|| {
        let allowed = crate::tracker::views::SmartView::ALL
            .iter()
            .map(|view| view.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        (
            StatusCode::BAD_REQUEST,
            format!("view must be one of {allowed}"),
        )
    })
}

fn trimmed_non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

async fn resolve_work_item_public_id(
    pool: &sqlx::PgPool,
    public_id: &str,
) -> Result<i64, (StatusCode, String)> {
    get_work_item_by_public_id(pool, public_id)
        .await
        .map_err(work_items_query_error)?
        .map(|row| row.id)
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                format!("unknown plan_public_id '{public_id}'"),
            )
        })
}

fn work_items_view_filter<'a>(
    view: crate::tracker::views::SmartView,
    assignee: Option<&'a str>,
    limit: i64,
) -> WorkItemFilter<'a> {
    match view {
        crate::tracker::views::SmartView::MyWork => WorkItemFilter {
            assignee,
            limit,
            ..Default::default()
        },
        crate::tracker::views::SmartView::NeedsTriage => WorkItemFilter {
            needs_triage: true,
            limit,
            ..Default::default()
        },
        crate::tracker::views::SmartView::Overdue => WorkItemFilter {
            overdue: true,
            limit,
            ..Default::default()
        },
        crate::tracker::views::SmartView::Blocked => WorkItemFilter {
            status: Some("blocked"),
            limit,
            ..Default::default()
        },
        crate::tracker::views::SmartView::NextActionable => WorkItemFilter {
            next_actionable: true,
            limit,
            ..Default::default()
        },
    }
}

fn work_items_query_error(e: sqlx::Error) -> (StatusCode, String) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("work item query failed: {e}"),
    )
}

/// Validate an optional `kind` filter against the closed [`WorkItemKind`]
/// vocabulary, returning the canonical `&'static str` (or `None` when
/// absent/blank). An unknown kind is a `400` listing the accepted values.
///
/// [`WorkItemKind`]: crate::tracker::kind::WorkItemKind
fn parse_work_item_kind_filter(
    raw: Option<&str>,
) -> Result<Option<&'static str>, (StatusCode, String)> {
    match trimmed_non_empty(raw) {
        None => Ok(None),
        Some(k) => crate::tracker::kind::WorkItemKind::parse(k)
            .map(|kind| Some(kind.as_str()))
            .ok_or_else(|| {
                let allowed = crate::tracker::kind::WorkItemKind::ALL
                    .iter()
                    .map(|kind| kind.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                (
                    StatusCode::BAD_REQUEST,
                    format!("unknown kind '{k}' (must be one of {allowed})"),
                )
            }),
    }
}

/// Validate an optional `status` filter against the closed [`WorkItemStatus`]
/// vocabulary. An unknown status is a `400` listing the accepted values.
///
/// [`WorkItemStatus`]: crate::tracker::status::WorkItemStatus
fn parse_work_item_status_filter(
    raw: Option<&str>,
) -> Result<Option<&'static str>, (StatusCode, String)> {
    match trimmed_non_empty(raw) {
        None => Ok(None),
        Some(s) => crate::tracker::status::WorkItemStatus::parse(s)
            .map(|status| Some(status.as_str()))
            .ok_or_else(|| {
                let allowed = crate::tracker::status::WorkItemStatus::ALL
                    .iter()
                    .map(|status| status.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                (
                    StatusCode::BAD_REQUEST,
                    format!("unknown status '{s}' (must be one of {allowed})"),
                )
            }),
    }
}

/// Resolve an optional `project` name filter to its id. A supplied-but-unknown
/// name is a `400` (rather than silently widening to every project — the
/// `resolve_project_id` "unknown => None" convention would otherwise drop the
/// filter). Absent/blank yields `None` (unconstrained).
async fn resolve_work_items_project(
    pool: &sqlx::PgPool,
    raw: Option<&str>,
) -> Result<Option<i32>, (StatusCode, String)> {
    match trimmed_non_empty(raw) {
        None => Ok(None),
        Some(name) => match resolve_project_id(pool, Some(name))
            .await
            .map_err(work_items_query_error)?
        {
            Some(id) => Ok(Some(id)),
            None => Err((StatusCode::BAD_REQUEST, format!("unknown project '{name}'"))),
        },
    }
}

/// Upper bound on timeline events returned by the work-item detail endpoint.
/// `work_item_timeline` clamps to `[1, 1000]`; 200 keeps a rich-but-bounded feed.
const WORK_ITEM_DETAIL_TIMELINE_LIMIT: i64 = 200;

/// Default subtree row cap for the tree endpoint when `?limit=` is omitted.
/// `get_work_item_subtree` clamps to `[1, 100_000]`; 1000 bounds the default
/// payload while remaining ample for realistic plan hierarchies.
const WORK_ITEM_TREE_DEFAULT_ROWS: i64 = 1000;

/// Response for `GET /api/work_items/{public_id}` — the item spine composed with
/// its timeline, acceptance criteria, and (for `kind='bug'` only) the bug-detail
/// sidecar. `bug_details` is `null` for every non-bug kind.
#[derive(Debug, Serialize)]
pub struct WorkItemDetailResponse {
    pub item: WorkItemRow,
    pub timeline: Vec<TimelineRow>,
    pub acceptance_criteria: Vec<AcceptanceCriterionRow>,
    pub bug_details: Option<BugDetailsRow>,
}

/// `GET /api/work_items/{public_id}` — one item plus its composed detail feeds.
/// 404 if no item carries that `public_id`. `pub(crate)`: only `daemon.rs` routes
/// it, and it is not part of the library's public API.
pub(crate) async fn work_item_detail(
    State(state): State<ApiState>,
    Path(public_id): Path<String>,
) -> Result<Json<WorkItemDetailResponse>, (StatusCode, String)> {
    let pool = real_pool(&state, "work item detail")?;
    let public_id = public_id.trim();
    if public_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "public_id must not be empty".to_string(),
        ));
    }
    let item = get_work_item_by_public_id(pool, public_id)
        .await
        .map_err(work_items_query_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("no work item '{public_id}'")))?;

    let timeline = work_item_timeline(pool, item.id, WORK_ITEM_DETAIL_TIMELINE_LIMIT)
        .await
        .map_err(work_items_query_error)?;
    let acceptance_criteria = list_acceptance_criteria(pool, item.id)
        .await
        .map_err(work_items_query_error)?;
    // The bug-detail sidecar exists only for bugs; skip the probe for other kinds.
    let bug_details = if item.kind.as_str() == crate::tracker::kind::WorkItemKind::Bug.as_str() {
        fetch_bug_details(pool, item.id)
            .await
            .map_err(work_items_query_error)?
    } else {
        None
    };

    Ok(Json(WorkItemDetailResponse {
        item,
        timeline,
        acceptance_criteria,
        bug_details,
    }))
}

/// Query for `GET /api/work_items/tree`.
#[derive(Debug, Deserialize)]
pub struct WorkItemTreeQuery {
    /// The subtree root's `public_id`. Required — omitting it is a `400` (list
    /// the top-level roots via `GET /api/work_items?kind=plan`).
    #[serde(default)]
    pub root: Option<String>,
    /// Optional cap on returned rows (default `WORK_ITEM_TREE_DEFAULT_ROWS`,
    /// clamped by the query to `[1, 100_000]`).
    #[serde(default)]
    pub limit: Option<i64>,
}

/// One node of the flattened subtree: every [`WorkItemRow`] field (via
/// `#[serde(flatten)]`) plus its `depth` (0 at the root) and `path` (numeric ids
/// from the root down to and including this node; `path.len() == depth + 1`).
#[derive(Debug, Serialize)]
pub struct WorkItemTreeNode {
    #[serde(flatten)]
    pub item: WorkItemRow,
    pub depth: i32,
    pub path: Vec<i64>,
}

/// Response for `GET /api/work_items/tree?root=<public_id>`.
#[derive(Debug, Serialize)]
pub struct WorkItemTreeResponse {
    /// The resolved root `public_id` (echoed from the DB row).
    pub root: String,
    pub count: usize,
    pub nodes: Vec<WorkItemTreeNode>,
}

/// `GET /api/work_items/tree?root=<public_id>` — the item's subtree, ordered by
/// depth then priority, flattened with a derived `depth`/`path` per node for
/// indentation and ancestry. `400` if `root` is omitted; `404` if it is unknown.
/// `pub(crate)`: only `daemon.rs` routes it.
pub(crate) async fn work_item_tree(
    State(state): State<ApiState>,
    Query(params): Query<WorkItemTreeQuery>,
) -> Result<Json<WorkItemTreeResponse>, (StatusCode, String)> {
    let pool = real_pool(&state, "work item tree")?;
    let root = trimmed_non_empty(params.root.as_deref()).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "tree requires a 'root' query parameter (a work-item public_id); \
             list the top-level roots via GET /api/work_items?kind=plan"
                .to_string(),
        )
    })?;
    let max_rows = params
        .limit
        .unwrap_or(WORK_ITEM_TREE_DEFAULT_ROWS)
        .clamp(1, 100_000);

    let root_item = get_work_item_by_public_id(pool, root)
        .await
        .map_err(work_items_query_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("no work item '{root}'")))?;

    let rows = get_work_item_subtree(pool, root_item.id, max_rows)
        .await
        .map_err(work_items_query_error)?;

    // Derive depth + root-path from the parent_id chain. `get_work_item_subtree`
    // returns rows ordered by ascending depth, so a node's parent is always
    // resolved before the node itself — a single O(n) forward pass. Sizes are
    // known, so preallocate.
    let mut depth_by_id: std::collections::HashMap<i64, i32> =
        std::collections::HashMap::with_capacity(rows.len());
    let mut path_by_id: std::collections::HashMap<i64, Vec<i64>> =
        std::collections::HashMap::with_capacity(rows.len());
    let mut nodes: Vec<WorkItemTreeNode> = Vec::with_capacity(rows.len());
    for item in rows {
        let (depth, path) = match item
            .parent_id
            .and_then(|pid| depth_by_id.get(&pid).copied().map(|depth| (pid, depth)))
        {
            Some((pid, parent_depth)) => {
                let parent_path = path_by_id.get(&pid);
                let mut path = Vec::with_capacity(parent_path.map_or(1, |p| p.len() + 1));
                if let Some(p) = parent_path {
                    path.extend_from_slice(p);
                }
                path.push(item.id);
                (parent_depth + 1, path)
            }
            // The subtree root: its parent (if any) is outside the returned set.
            None => (0, vec![item.id]),
        };
        depth_by_id.insert(item.id, depth);
        path_by_id.insert(item.id, path.clone());
        nodes.push(WorkItemTreeNode { item, depth, path });
    }

    Ok(Json(WorkItemTreeResponse {
        root: root_item.public_id,
        count: nodes.len(),
        nodes,
    }))
}

fn filter_mandate_bundle_by_scope(
    bundle: &mut crate::mandates::MandateBundle,
    scope: &str,
) -> Result<(), (StatusCode, String)> {
    let scope = scope.trim().to_ascii_lowercase();
    if scope.is_empty() || scope == "all" {
        return Ok(());
    }
    match scope.as_str() {
        "global" | "workspace" | "project" | "session" => {
            bundle.sources.retain(|source| source.scope == scope);
            bundle
                .skipped_sources
                .retain(|source| source.scope == scope);
            if scope != "project" {
                bundle.project_override = None;
            }
            Ok(())
        }
        _ => Err((
            StatusCode::BAD_REQUEST,
            "scope must be one of all, global, workspace, project, session".to_string(),
        )),
    }
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
    /// Lifecycle phase label (starting / scanning / ready / …) — the same value
    /// `/health` reports, surfaced on the Overview status pane.
    pub phase: &'static str,
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
        phase: state.lifecycle.current().label(),
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

// ============================================================================
// GET /api/stats?kind=... — Web UI stats slices
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct StatsQuery {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub job: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub since_minutes: Option<i32>,
    /// Clients view: include already-exited clients (default false → live only).
    #[serde(default)]
    pub include_exited: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub kind: String,
    pub server_seq: Option<i64>,
    pub data: serde_json::Value,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
struct ToolTelemetryRollup {
    tool: String,
    calls: i64,
    ok_count: i64,
    error_count: i64,
    avg_duration_ms: Option<f64>,
    max_duration_ms: Option<i64>,
    last_ts: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn stats(
    State(state): State<ApiState>,
    Query(params): Query<StatsQuery>,
) -> Result<Json<StatsResponse>, (StatusCode, String)> {
    let kind = params
        .kind
        .unwrap_or_else(|| "status".to_string())
        .trim()
        .to_ascii_lowercase();
    let server_seq = if let Some(pool) = state.db.pool() {
        current_realtime_seq(pool).await.ok()
    } else {
        None
    };
    let data = match kind.as_str() {
        "counters" => state.stats.snapshot(),
        "status" => {
            let Json(response) = status(State(state.clone())).await?;
            serde_json::to_value(response).map_err(json_error)?
        }
        "index" => {
            let pool = real_pool(&state, "index stats")?;
            let snapshot = status_snapshot(pool).await.map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("status_snapshot failed: {}", e),
                )
            })?;
            let failures = state.db.failure_kind_counts().await.map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failure_kind_counts failed: {}", e),
                )
            })?;
            serde_json::json!({
                "project_count": snapshot.project_count,
                "indexed_file_count": snapshot.indexed_file_count,
                "chunk_count": snapshot.chunk_count,
                "last_indexed_at": snapshot.last_indexed_at,
                "per_project": snapshot.per_project,
                "failure_kind_counts": failures,
            })
        }
        "cron" => {
            let pool = real_pool(&state, "cron stats")?;
            let limit = params.limit.unwrap_or(50).clamp(1, 500);
            let rollup = crate::db::queries::cron_job_rollup(pool)
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("cron_job_rollup failed: {}", e),
                    )
                })?;
            let recent = crate::db::queries::recent_cron_runs(pool, params.job.as_deref(), limit)
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("recent_cron_runs failed: {}", e),
                    )
                })?;
            serde_json::json!({ "rollup": rollup, "recent": recent })
        }
        "clients" => {
            let pool = real_pool(&state, "client stats")?;
            let since_minutes = params.since_minutes.unwrap_or(240).clamp(1, 10_080);
            let active = crate::db::queries::active_clients(
                pool,
                params.project.as_deref(),
                params.include_exited.unwrap_or(false),
            )
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("active_clients failed: {}", e),
                )
            })?;
            let matrix = crate::db::queries::client_project_matrix(
                pool,
                since_minutes,
                params.project.as_deref(),
            )
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("client_project_matrix failed: {}", e),
                )
            })?;
            serde_json::json!({ "active": active, "project_matrix": matrix })
        }
        "telemetry" => {
            let pool = real_pool(&state, "telemetry stats")?;
            let limit = params.limit.unwrap_or(25).clamp(1, 100);
            let since_minutes = params.since_minutes.unwrap_or(240).clamp(1, 10_080);
            let rows = sqlx::query_as::<_, ToolTelemetryRollup>(
                "SELECT tool,
                        COUNT(*)::BIGINT AS calls,
                        COUNT(*) FILTER (WHERE outcome = 'ok')::BIGINT AS ok_count,
                        COUNT(*) FILTER (WHERE outcome <> 'ok')::BIGINT AS error_count,
                        AVG(duration_ms)::float8 AS avg_duration_ms,
                        MAX(duration_ms)::BIGINT AS max_duration_ms,
                        MAX(ts) AS last_ts
                   FROM mcp_tool_calls
                  WHERE ts > now() - make_interval(mins => $1)
                  GROUP BY tool
                  ORDER BY calls DESC, tool
                  LIMIT $2",
            )
            .bind(since_minutes)
            .bind(limit)
            .fetch_all(pool)
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("telemetry rollup failed: {}", e),
                )
            })?;
            serde_json::json!({ "tools": rows })
        }
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                "kind must be one of status, counters, index, cron, clients, telemetry".to_string(),
            ));
        }
    };

    Ok(Json(StatsResponse {
        kind,
        server_seq,
        data,
    }))
}

fn real_pool<'a>(
    state: &'a ApiState,
    surface: &str,
) -> Result<&'a sqlx::PgPool, (StatusCode, String)> {
    state.db.pool().ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{surface} requires a real PgPool DbClient (mock unsupported)"),
        )
    })
}

fn json_error(e: serde_json::Error) -> (StatusCode, String) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("json serialization failed: {e}"),
    )
}
