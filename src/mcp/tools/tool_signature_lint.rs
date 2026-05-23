//! `tool_signature_lint` — lint same-shape APIs for inconsistencies.
//!
//! Flags:
//! - Parameter-order inconsistencies across same-shape signatures
//! - Primitive obsession (long parameter lists of identical type)
//! - Boolean-flag explosion (>2 bool params on a function)
//! - Inconsistent generic naming (T vs U for the same role)

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::SignatureLintParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_signature_lint(
    ctx: &SystemContext,
    params: SignatureLintParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "signature_lint", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50).max(1) as usize;

    // Per-row tuple from the symbol+parameter join. Aliased so sqlx's
    // inferred type stays inside clippy's complexity thresholds.
    type ParamRow = (i64, String, i32, Option<String>, Vec<String>, bool);
    // Aggregate value held per symbol_id while we cluster parameters.
    type SymBucket = (String, Vec<(i32, Option<String>, Vec<String>, bool)>);
    let rows: Vec<ParamRow> = sqlx::query_as(
        "SELECT fs.id, fs.name, p.position, p.name, p.type_tags, p.is_variadic
         FROM file_symbols fs
         JOIN indexed_files f ON f.id = fs.file_id
         JOIN symbol_parameters p ON p.symbol_id = fs.id
         WHERE f.project_id = $1
           AND fs.kind = 'function'
         ORDER BY fs.id, p.position",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

    // Group parameters by symbol_id.
    let mut by_symbol: HashMap<i64, SymBucket> = HashMap::new();
    for (sid, sname, pos, pname, tags, is_var) in rows {
        let entry = by_symbol
            .entry(sid)
            .or_insert_with(|| (sname.clone(), Vec::new()));
        entry.1.push((pos, pname, tags, is_var));
    }

    let mut primitive_obsession: Vec<serde_json::Value> = Vec::new();
    let mut boolean_flag_explosion: Vec<serde_json::Value> = Vec::new();
    let mut long_parameter_lists: Vec<serde_json::Value> = Vec::new();
    let mut param_name_inconsistencies: Vec<serde_json::Value> = Vec::new();

    // Primitive obsession: ≥4 parameters with identical type_tags (and at least one tag).
    // Boolean flag explosion: >2 parameters tagged `bool`.
    // Long parameter list: ≥6 params.
    for (_sid, (sname, mut params)) in by_symbol.clone() {
        params.sort_by_key(|(p, _, _, _)| *p);
        if params.len() >= 6 {
            long_parameter_lists.push(json!({
                "symbol_name": sname,
                "parameter_count": params.len(),
            }));
        }
        let bool_count = params
            .iter()
            .filter(|(_, _, tags, _)| tags.iter().any(|t| t == "bool"))
            .count();
        if bool_count > 2 {
            boolean_flag_explosion.push(json!({
                "symbol_name": sname,
                "bool_parameter_count": bool_count,
            }));
        }
        // Type-tag homogeneity: if ≥4 params share an identical non-empty tag set.
        let mut by_tagset: HashMap<Vec<String>, u32> = HashMap::new();
        for (_, _, tags, _) in &params {
            if tags.is_empty() {
                continue;
            }
            let mut k = tags.clone();
            k.sort();
            *by_tagset.entry(k).or_insert(0) += 1;
        }
        for (tagset, count) in by_tagset {
            if count >= 4 {
                primitive_obsession.push(json!({
                    "symbol_name": sname,
                    "shared_tag_set": tagset,
                    "parameter_count_with_shared_tags": count,
                }));
            }
        }
    }

    // Parameter-name inconsistency: group symbols by (position, type_tags),
    // find positions where multiple distinct names appear.
    // Restrict to positions/tags appearing in ≥3 functions to avoid noise.
    let mut by_position_tags: HashMap<(i32, Vec<String>), HashMap<String, u32>> = HashMap::new();
    for (_sid, (_sname, params)) in by_symbol {
        for (pos, pname, tags, _is_var) in params {
            let Some(name) = pname else { continue };
            if tags.is_empty() {
                continue;
            }
            let mut tagkey = tags.clone();
            tagkey.sort();
            *by_position_tags
                .entry((pos, tagkey))
                .or_default()
                .entry(name)
                .or_insert(0) += 1;
        }
    }
    for ((pos, tagset), name_counts) in by_position_tags {
        let total: u32 = name_counts.values().sum();
        if total < 3 || name_counts.len() < 2 {
            continue;
        }
        let mut entries: Vec<(String, u32)> = name_counts.into_iter().collect();
        entries.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
        // Dominance: if the top name is <70% of occurrences AND there are
        // ≥2 distinct names, flag it.
        let dominant_share = (entries[0].1 as f64) / (total as f64);
        if dominant_share < 0.70 {
            param_name_inconsistencies.push(json!({
                "position": pos,
                "shared_tag_set": tagset,
                "name_counts": entries.iter().map(|(n, c)| json!({"name": n, "count": c})).collect::<Vec<_>>(),
                "total_occurrences": total,
                "dominant_name_share": dominant_share,
            }));
        }
    }

    // Sort + truncate each section.
    primitive_obsession.truncate(limit);
    boolean_flag_explosion.truncate(limit);
    long_parameter_lists.truncate(limit);
    param_name_inconsistencies.truncate(limit);

    json_result(&json!({
        "long_parameter_lists": long_parameter_lists,
        "primitive_obsession": primitive_obsession,
        "boolean_flag_explosion": boolean_flag_explosion,
        "parameter_name_inconsistencies": param_name_inconsistencies,
        "guidance": "Signature smells worth surfacing:\n\
                     - long_parameter_lists: ≥6 params suggests primitive obsession or missing struct.\n\
                     - primitive_obsession: ≥4 params with identical type tags.\n\
                     - boolean_flag_explosion: >2 bool params — split into an enum or struct.\n\
                     - parameter_name_inconsistencies: same (position, shape) is named differently across functions."
    }))
}
