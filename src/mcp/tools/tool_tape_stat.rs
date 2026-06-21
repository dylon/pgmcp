//! `tape_stat` — residency statistics for the per-tree tape store.
//!
//! Reports the store's live accounting: `resident_bytes`
//! ([`TapeStore::resident_bytes`](context_tape::TapeStore::resident_bytes), the
//! Σ of resident page content lengths), `n_pages` (resident page count),
//! `n_dirty` (pages awaiting write-back), and `n_ooc_segments` (out-of-core
//! overlay segments holding spilled cold-clean pages). Querying a tree that has
//! never been touched lazily creates an empty store and reports zeros.
//!
//! Boundary: analytical, no shell/exec; reads only; never writes the user's
//! files.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::TapeStatParams;
use crate::mcp::tools::sota_helpers::json_result;
use crate::mcp::tools::tape_support::{tree_id_of, tree_path_of};

pub async fn tool_tape_stat(
    ctx: &SystemContext,
    params: TapeStatParams,
) -> Result<CallToolResult, McpError> {
    let tree_id = tree_id_of(&params.tree);

    let (resident_bytes, n_pages, n_dirty, n_ooc_segments) =
        ctx.tape_registry().with_store(tree_id, |s| {
            (
                s.resident_bytes(),
                s.len(),
                s.dirty_len(),
                s.overlay().segment_count(),
            )
        });

    json_result(&json!({
        "tree": params.tree,
        "tree_path": tree_path_of(&params.tree).0,
        "tree_id": tree_id.to_string(),
        "resident_bytes": resident_bytes,
        "n_pages": n_pages,
        "n_dirty": n_dirty,
        "n_ooc_segments": n_ooc_segments,
    }))
}
