//! `tape_slice` — positional range scan over the per-tree tape store.
//!
//! Yields every resident page whose canonical key lies in the inclusive address
//! range `[lo, hi]`, **in address (key) order** (the trie's depth-first order),
//! via [`TapeStore::slice`](context_tape::TapeStore::slice). `lo`/`hi` are
//! address path strings (== `PageAddress::to_path()`). A scan that reaches
//! `max_pages` sets `truncated=true`. If `lo > hi` the range is empty.
//!
//! Boundary: analytical, no shell/exec; reads only (resident pages); never
//! writes the user's files.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::TapeSliceParams;
use crate::mcp::tools::sota_helpers::json_result;
use crate::mcp::tools::tape_support::{parse_address, render_address, tree_id_of};

/// Default cap on pages returned by one slice.
const DEFAULT_SLICE_MAX: usize = 64;

pub async fn tool_tape_slice(
    ctx: &SystemContext,
    params: TapeSliceParams,
) -> Result<CallToolResult, McpError> {
    let lo = parse_address(&params.lo)?;
    let hi = parse_address(&params.hi)?;
    let tree_id = tree_id_of(&params.tree);
    let max_pages = params.max_pages.unwrap_or(DEFAULT_SLICE_MAX).max(1);

    // Take one extra so we can tell whether the range overflowed the cap.
    let (pages, truncated) = ctx.tape_registry().with_store(tree_id, |s| {
        let mut out = Vec::with_capacity(max_pages);
        let mut overflow = false;
        for (addr, page) in s.slice(&lo, &hi) {
            if out.len() == max_pages {
                overflow = true;
                break;
            }
            out.push(json!({
                "address": render_address(&addr),
                "content": page.content,
                "est_tokens": page.meta.est_tokens,
                "dirty": page.meta.dirty,
            }));
        }
        (out, overflow)
    });

    json_result(&json!({
        "tree": params.tree,
        "lo": render_address(&lo),
        "hi": render_address(&hi),
        "pages": pages,
        "truncated": truncated,
    }))
}
