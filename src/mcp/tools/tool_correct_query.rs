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
use crate::cron::ngram_lm_train::model_path_for;
use crate::fuzzy::sync::open_symbol_trie;
use crate::mcp::server::CorrectQueryParams;
use crate::mcp::tools::sota_helpers::json_result;
use crate::wfst::correction::correct_query_single;
use crate::wfst::hybrid_lm::PgmcpHybridLm;

pub async fn run(
    ctx: &SystemContext,
    params: CorrectQueryParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let max_d = params.max_distance.unwrap_or(2) as usize;
    let lm_weight = params.lm_weight.unwrap_or(0.5);

    let (data_dir, phonetic_cost_weight, phonetic_max_total_cost) = {
        let cfg = ctx.config().load();
        (
            cfg.fuzzy.data_dir.clone(),
            cfg.fuzzy.phonetic_cost_weight,
            cfg.fuzzy.phonetic_max_total_cost,
        )
    };

    // Per-project symbol vocabulary (lazy-warmed from PG on first call).
    let idx = open_symbol_trie(ctx, &params.project).await?;

    // Optional per-project n-gram LM. Absent → edit + phonetic scoring only.
    let model_path = model_path_for(&data_dir, &params.project);
    let lm = if model_path.exists() {
        PgmcpHybridLm::open(&model_path).ok()
    } else {
        None
    };

    let result = correct_query_single(
        &params.query,
        max_d,
        1.0,
        lm_weight,
        phonetic_cost_weight,
        phonetic_max_total_cost,
        &idx,
        lm.as_ref(),
    );

    json_result(&json!({
        "input": result.input,
        "corrected": result.corrected,
        "changed": result.changed,
        "confidence": result.confidence,
        "used_lm": result.used_lm,
        "model_available": lm.is_some(),
    }))
}
