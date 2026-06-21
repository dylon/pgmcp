//! `tape_get` — fetch one page's situated bytes from the per-tree tape.
//!
//! Resolution cascade (the P3 [`RealTapeDataPlane::get`] contract): resident hot
//! tier → out-of-core overlay → hydrate the **READ-ONLY** corpus and admit the
//! page *clean*. A non-resident `Scratch` page has no corpus backing and is a
//! benign `not_found`. The `dirty` flag reports whether the resident copy has an
//! un-written-back agent edit.
//!
//! Boundary: analytical, no shell/exec; reads only (hydrate is READ-ONLY); never
//! writes the user's files.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::TapeGetParams;
use crate::mcp::tools::sota_helpers::json_result;
use crate::mcp::tools::tape_support::{parse_address, render_address, tree_id_of, tree_path_of};
use crate::tape::data_plane::{TapeDataPlane, TapeError};
use crate::tape::real_data_plane::RealTapeDataPlane;

pub async fn tool_tape_get(
    ctx: &SystemContext,
    params: TapeGetParams,
) -> Result<CallToolResult, McpError> {
    let address = parse_address(&params.address)?;
    let tree_path = tree_path_of(&params.tree);
    let tree_id = tree_id_of(&params.tree);
    let addr_path = render_address(&address);

    // Whether the resident copy (if any) is dirty — a property of the per-tree
    // store, orthogonal to where the bytes came from.
    let dirty = ctx
        .tape_registry()
        .with_store(tree_id, |s| s.is_dirty(&address));

    // Prefer the real data plane (full hot → OOC → hydrate cascade). In mock-DB
    // / CLI mode it is unavailable, so fall back to the resident-only fast path
    // (the corpus simply cannot be hydrated without a live pool).
    match RealTapeDataPlane::from_context(ctx) {
        Some(plane) => {
            let addr = crate::tape::working_set::PageAddr(addr_path.clone());
            match plane.get(&tree_path, &addr).await {
                Ok(content) => json_result(&json!({
                    "tree": params.tree,
                    "address": addr_path,
                    "content": content.bytes,
                    "est_tokens": content.est_tokens,
                    "dirty": dirty,
                })),
                Err(TapeError::NotFound(p)) => json_result(&json!({
                    "tree": params.tree,
                    "address": addr_path,
                    "found": false,
                    "reason": format!("page not resident and not hydratable: {p}"),
                })),
                Err(TapeError::Backend(e)) => Err(McpError::internal_error(
                    format!("tape_get backend error: {e}"),
                    None,
                )),
            }
        }
        None => {
            // Resident-only path: hot/OOC cascade in the tree store, no hydrate.
            match ctx
                .tape_registry()
                .with_store(tree_id, |s| s.get_cascade(&address))
            {
                Some(page) => json_result(&json!({
                    "tree": params.tree,
                    "address": addr_path,
                    "content": page.content,
                    "est_tokens": page.meta.est_tokens,
                    "dirty": dirty,
                })),
                None => json_result(&json!({
                    "tree": params.tree,
                    "address": addr_path,
                    "found": false,
                    "reason": "page not resident (corpus hydrate unavailable in this mode)",
                })),
            }
        }
    }
}
