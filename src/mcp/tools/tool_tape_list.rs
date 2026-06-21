//! `tape_list` — enumerate resident page addresses in the per-tree store.
//!
//! Walks the path index
//! ([`AddressIndex::resolve_path_prefix`](context_tape::AddressIndex::resolve_path_prefix))
//! for every resident address whose path begins with `prefix` (an empty / omitted
//! prefix lists them all), returned in address (key) order, capped by `limit`.
//! Also reports `dirty_count` — how many resident pages await write-back.
//!
//! Boundary: analytical, no shell/exec; reads only (resident page paths); never
//! writes the user's files.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::TapeListParams;
use crate::mcp::tools::sota_helpers::json_result;
use crate::mcp::tools::tape_support::{render_address, tree_id_of};

/// Default cap on addresses returned.
const DEFAULT_LIST_LIMIT: usize = 256;

pub async fn tool_tape_list(
    ctx: &SystemContext,
    params: TapeListParams,
) -> Result<CallToolResult, McpError> {
    let limit = params.limit.unwrap_or(DEFAULT_LIST_LIMIT).max(1);
    let prefix = params.prefix.as_deref().unwrap_or("");
    let tree_id = tree_id_of(&params.tree);

    let (addresses, total, dirty_count) = ctx.tape_registry().with_store(tree_id, |s| {
        let all = s.index().resolve_path_prefix(prefix);
        let total = all.len();
        let truncated: Vec<String> = all
            .into_iter()
            .take(limit)
            .map(|a| render_address(&a))
            .collect();
        (truncated, total, s.dirty_len())
    });

    json_result(&json!({
        "tree": params.tree,
        "prefix": prefix,
        "addresses": addresses,
        "returned": addresses.len(),
        "total_matching": total,
        "truncated": total > addresses.len(),
        "dirty_count": dirty_count,
    }))
}
