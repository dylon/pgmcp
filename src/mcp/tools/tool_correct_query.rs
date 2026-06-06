//! `tool_correct_query` — single-shot query correction over a project's
//! persistent symbol vocabulary + per-project n-gram language model.
//!
//! Routes through pgmcp's own WFST corrector (`wfst::correction`), NOT
//! llammer-pipeline: the latter's LM-rerank layer is a stub, so it could
//! never apply a project language model. Here candidates come from the
//! persistent symbol trie (lazy-warmed from PG), the correction lattice
//! blends edit + phonetic cost (G3), and — when a trained per-project model
//! exists — the Modified-Kneser-Ney `PgmcpHybridLm` rescores the lattice.
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::cron::ngram_lm_train::model_path_for_project;
use crate::fuzzy::limits::bounded_max_distance;
use crate::fuzzy::sync::open_symbol_trie;
use crate::mcp::server::CorrectQueryParams;
use crate::mcp::tools::sota_helpers::{json_result, project_id_or_err};
use crate::wfst::correction::correct_query_single;
use crate::wfst::hybrid_lm::PgmcpHybridLm;

const CORRECT_QUERY_MAX_CHARS: usize = 4096;
const DEFAULT_LM_WEIGHT: f64 = 0.5;

fn normalize_query(raw: &str) -> Result<String, McpError> {
    let query = raw.trim();
    if query.is_empty() {
        return Err(McpError::invalid_params("query must be non-empty", None));
    }
    if query.chars().count() > CORRECT_QUERY_MAX_CHARS {
        return Err(McpError::invalid_params(
            format!("query must be at most {CORRECT_QUERY_MAX_CHARS} characters"),
            None,
        ));
    }
    Ok(query.to_string())
}

fn normalize_lm_weight(raw: Option<f64>) -> Result<f64, McpError> {
    let weight = raw.unwrap_or(DEFAULT_LM_WEIGHT);
    if !weight.is_finite() {
        return Err(McpError::invalid_params("lm_weight must be finite", None));
    }
    Ok(weight.clamp(0.0, 1.0))
}

pub async fn run(
    ctx: &SystemContext,
    params: CorrectQueryParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project = params.project.trim().to_string();
    let project_id = project_id_or_err(ctx, &project).await?;
    let query = normalize_query(&params.query)?;
    let max_d = bounded_max_distance(params.max_distance);
    let lm_weight = normalize_lm_weight(params.lm_weight)?;

    let (data_dir, phonetic_cost_weight, phonetic_max_total_cost) = {
        let cfg = ctx.config().load();
        (
            cfg.fuzzy.data_dir.clone(),
            cfg.fuzzy.phonetic_cost_weight,
            cfg.fuzzy.phonetic_max_total_cost,
        )
    };

    // Per-project symbol vocabulary (lazy-warmed from PG on first call).
    let idx = open_symbol_trie(ctx, &project).await?;

    // Optional per-project n-gram LM. Absent → edit + phonetic scoring only.
    let model_path = model_path_for_project(&data_dir, project_id, &project);
    let lm = if model_path.exists() {
        PgmcpHybridLm::open(&model_path).ok()
    } else {
        None
    };

    let result = correct_query_single(
        &query,
        max_d,
        1.0,
        lm_weight,
        phonetic_cost_weight,
        phonetic_max_total_cost,
        &idx,
        lm.as_ref(),
    );

    json_result(&json!({
        "project": project,
        "input": result.input,
        "corrected": result.corrected,
        "changed": result.changed,
        "confidence": result.confidence,
        "used_lm": result.used_lm,
        "model_available": lm.is_some(),
        "max_distance": max_d,
        "lm_weight": lm_weight,
    }))
}
