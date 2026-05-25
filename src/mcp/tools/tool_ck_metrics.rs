//! `tool_ck_metrics` — Chidamber-Kemerer OO metrics (graph-roadmap Phase 4.3).
//!
//! Per class (an OO `file_symbols` kind), the full CK suite:
//! - **WMC** Weighted Methods per Class = Σ method cyclomatic (real AST CC),
//! - **DIT** Depth of Inheritance Tree, **NOC** Number Of Children — from the
//!   `inherit`/`impl` edges the symbol extractors emit (Python, C/C++ today),
//! - **CBO** Coupling Between Objects ≈ distinct target files the class touches,
//! - **RFC** Response For Class = methods + distinct call targets.
//!
//! DIT/NOC are 0 for languages whose backend doesn't yet emit inheritance edges
//! (the per-language follow-up); WMC/CBO/RFC populate from existing data for any
//! parsed language.

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::code_analysis::ck_metrics::dit_noc;
use crate::context::SystemContext;
use crate::mcp::server::CkMetricsParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_ck_metrics(
    ctx: &SystemContext,
    params: CkMetricsParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "ck_metrics", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(40).clamp(1, 1000) as usize;
    let sort = params.sort.as_deref().unwrap_or("wmc");

    let classes = crate::db::queries::ck_class_rows(pool, project_id)
        .await
        .map_err(|e| McpError::internal_error(format!("CK class query failed: {}", e), None))?;
    if classes.is_empty() {
        return json_result(&json!({
            "project": params.project,
            "classes": [],
            "guidance": "No OO classes found (or symbol-extraction cron hasn't run). CK metrics need \
                         file_symbols of kind class/struct/interface/trait/enum."
        }));
    }

    let edges = crate::db::queries::ck_inheritance_edges(pool, project_id)
        .await
        .map_err(|e| McpError::internal_error(format!("inheritance query failed: {}", e), None))?;
    let mut child_to_parents: HashMap<i64, Vec<i64>> = HashMap::new();
    for (child, parent) in edges {
        child_to_parents.entry(child).or_default().push(parent);
    }
    let class_ids: Vec<i64> = classes.iter().map(|c| c.symbol_id).collect();
    let dn = dit_noc(&class_ids, &child_to_parents);

    let mut rows: Vec<serde_json::Value> = classes
        .iter()
        .map(|c| {
            let d = dn.get(&c.symbol_id).cloned().unwrap_or_default();
            let rfc = c.method_count + c.distinct_callees;
            json!({
                "class": c.name,
                "file": c.relative_path,
                "wmc": c.wmc,
                "dit": d.dit,
                "noc": d.noc,
                "cbo": c.cbo,
                "rfc": rfc,
                "methods": c.method_count,
            })
        })
        .collect();

    let key = |v: &serde_json::Value| -> i64 {
        match sort {
            "dit" => v["dit"].as_i64().unwrap_or(0),
            "noc" => v["noc"].as_i64().unwrap_or(0),
            "cbo" => v["cbo"].as_i64().unwrap_or(0),
            "rfc" => v["rfc"].as_i64().unwrap_or(0),
            _ => v["wmc"].as_i64().unwrap_or(0),
        }
    };
    rows.sort_by(|a, b| key(b).cmp(&key(a)));
    rows.truncate(limit);

    let inheritance_available = !child_to_parents.is_empty();

    json_result(&json!({
        "project": params.project,
        "class_count": classes.len(),
        "sort": sort,
        "inheritance_edges_present": inheritance_available,
        "classes": rows,
        "guidance": "Chidamber-Kemerer suite (TSE 1994). WMC = Σ method cyclomatic (high = complex \
            class); DIT = inheritance depth (deep = fragile/hard to follow); NOC = direct subclasses \
            (high = a heavily-extended base, change-risky); CBO = distinct files coupled to (high = poor \
            encapsulation); RFC = methods + distinct calls (high = large response surface, hard to test). \
            DIT/NOC are 0 when `inheritance_edges_present=false` — the language's backend doesn't emit \
            inherit/impl edges yet (Python & C/C++ do). Sort via `sort`=wmc|dit|noc|cbo|rfc."
    }))
}
