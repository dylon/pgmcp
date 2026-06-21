//! `tape_peek` — a cheap head/size probe over a tape page WITHOUT materializing
//! its full content (mirrors [`crate::a2a::rlm`]'s `peek`).
//!
//! Returns a bounded head preview (default 256 bytes, truncated on a UTF-8 char
//! boundary), the page's total `size_bytes`, and `n_pages` — the number of
//! resident pages whose address path shares this address's path as a prefix (so
//! peeking `corpus/file/5` reports how many `corpus/file/5/…` pages are resident,
//! and peeking a single leaf reports 1). The probe is resident-only (hot/OOC
//! cascade); it never hydrates the corpus.
//!
//! Boundary: analytical, no shell/exec; reads only; never writes the user's
//! files.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::TapePeekParams;
use crate::mcp::tools::sota_helpers::json_result;
use crate::mcp::tools::tape_support::{
    head_on_boundary, parse_address, render_address, tree_id_of,
};

/// Default head-preview byte budget.
const DEFAULT_PEEK_BYTES: usize = 256;

pub async fn tool_tape_peek(
    ctx: &SystemContext,
    params: TapePeekParams,
) -> Result<CallToolResult, McpError> {
    let address = parse_address(&params.address)?;
    let tree_id = tree_id_of(&params.tree);
    let addr_path = render_address(&address);
    let want = params.bytes.unwrap_or(DEFAULT_PEEK_BYTES);

    // Resident page (hot/OOC cascade) — peek never hydrates.
    let resident = ctx
        .tape_registry()
        .with_store(tree_id, |s| s.get_cascade(&address));

    // n_pages: resident pages sharing this address's path as a prefix. Cheap —
    // a path-index prefix walk, not a content materialization.
    let prefix = addr_path.clone();
    let n_pages = ctx
        .tape_registry()
        .with_store(tree_id, |s| s.index().resolve_path_prefix(&prefix).len());

    match resident {
        Some(page) => {
            let (head, truncated) = head_on_boundary(&page.content, want);
            json_result(&json!({
                "tree": params.tree,
                "address": addr_path,
                "resident": true,
                "head": head,
                "head_truncated": truncated,
                "size_bytes": page.content.len(),
                "est_tokens": page.meta.est_tokens,
                "dirty": page.meta.dirty,
                "kind": kind_str(page.meta.kind),
                "n_pages": n_pages,
            }))
        }
        None => json_result(&json!({
            "tree": params.tree,
            "address": addr_path,
            "resident": false,
            "head": "",
            "size_bytes": 0,
            // A non-resident leaf still reports how many sibling pages under its
            // path prefix ARE resident (0 for a truly empty prefix).
            "n_pages": n_pages,
        })),
    }
}

/// Render the data-plane page kind for the response.
fn kind_str(kind: context_tape::PageKind) -> &'static str {
    match kind {
        context_tape::PageKind::FileChunk => "file_chunk",
        context_tape::PageKind::MemoryObservation => "memory_observation",
        context_tape::PageKind::SummaryNode => "summary_node",
        context_tape::PageKind::Scratch => "scratch",
    }
}
