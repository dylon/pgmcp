//! `tool_cross_language_api_equivalents` — find functions in different
//! languages whose signature shapes match.
//!
//! Reads from the `cross_language_signature_clones` materialized table
//! populated by `src/cron/cross_language_signatures.rs`. Returns
//! cross-language pairs ranked by similarity. Empty when the
//! materialized table hasn't been populated yet — the consumer can call
//! `trigger_cron` with `cross_language_signatures` to refresh.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::CrossLanguageApiEquivalentsParams;
use crate::mcp::tools::sema_helpers::equivalence::materialized_available;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_cross_language_api_equivalents(
    ctx: &SystemContext,
    params: CrossLanguageApiEquivalentsParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "cross_language_api_equivalents", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let min_similarity = params.min_similarity.unwrap_or(0.7).clamp(0.0, 1.0);
    let limit = params.limit.unwrap_or(50).max(1) as i64;

    if !materialized_available(pool).await.unwrap_or(false) {
        return json_result(&json!({
            "pairs": [],
            "guidance": "The `cross_language_signature_clones` materialized table is empty. \
                         Run `trigger_cron` with cron_name=\"cross_language_signatures\" to populate.",
        }));
    }

    type PairRow = (
        i64,
        i64,
        String,
        String,
        f32,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<PairRow> = sqlx::query_as(
        "SELECT c.symbol_id_a, c.symbol_id_b,
                c.language_a, c.language_b,
                c.similarity,
                fa.name, fa.scope_path,
                fb.name, fb.scope_path
         FROM cross_language_signature_clones c
         JOIN file_symbols fa ON fa.id = c.symbol_id_a
         JOIN file_symbols fb ON fb.id = c.symbol_id_b
         WHERE c.similarity >= $1::real
         ORDER BY c.similarity DESC
         LIMIT $2::int8",
    )
    .bind(min_similarity)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

    let pairs: Vec<serde_json::Value> = rows
        .into_iter()
        .map(
            |(sa, sb, lang_a, lang_b, sim, name_a, scope_a, name_b, scope_b)| {
                json!({
                    "similarity": sim,
                    "language_a": lang_a,
                    "language_b": lang_b,
                    "symbol_a": { "id": sa, "name": name_a, "scope_path": scope_a },
                    "symbol_b": { "id": sb, "name": name_b, "scope_path": scope_b },
                })
            },
        )
        .collect();

    json_result(&json!({
        "pairs": pairs,
        "min_similarity": min_similarity,
        "guidance": "Functions in different languages that share a structural signature shape. \
                     Useful for compiler validation (MeTTaTron → Rholang → Rust), \
                     porting audits, and cross-language API harmonization."
    }))
}
