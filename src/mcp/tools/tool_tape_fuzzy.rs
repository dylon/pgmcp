//! `tape_fuzzy` — Levenshtein fuzzy-path search over the per-tree store.
//!
//! Error-corrects an address path: returns every resident page whose
//! `PageAddress::to_path()` is within `max_distance` edits of `query` (standard
//! Levenshtein), ordered by ascending distance, via
//! [`AddressIndex::fuzzy_path`](context_tape::AddressIndex::fuzzy_path). An
//! optional `filter` keeps only matches whose path begins with that prefix.
//!
//! Boundary: analytical, no shell/exec; reads only (resident page paths); never
//! writes the user's files.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::TapeFuzzyParams;
use crate::mcp::tools::sota_helpers::json_result;
use crate::mcp::tools::tape_support::{render_address, tree_id_of};

/// Default maximum edit distance for a fuzzy-path query.
const DEFAULT_MAX_DISTANCE: usize = 2;

pub async fn tool_tape_fuzzy(
    ctx: &SystemContext,
    params: TapeFuzzyParams,
) -> Result<CallToolResult, McpError> {
    let max_distance = params.max_distance.unwrap_or(DEFAULT_MAX_DISTANCE);
    let tree_id = tree_id_of(&params.tree);
    let filter = params.filter.as_deref();

    let hits = ctx.tape_registry().with_store(tree_id, |s| {
        s.index()
            .fuzzy_path(&params.query, max_distance)
            .into_iter()
            .filter_map(|fa| {
                let path = render_address(&fa.addr);
                match filter {
                    Some(pre) if !path.starts_with(pre) => None,
                    _ => Some(json!({
                        "address": path,
                        "distance": fa.distance,
                    })),
                }
            })
            .collect::<Vec<_>>()
    });

    json_result(&json!({
        "tree": params.tree,
        "query": params.query,
        "max_distance": max_distance,
        "hits": hits,
    }))
}
