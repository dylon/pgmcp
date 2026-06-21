//! `tape_put` — stage page content into the per-tree tape store as DIRTY.
//!
//! Omitting `address` mints a fresh tree-local [`PageAddress::Scratch`] slot (a
//! random 16-byte UUID slot, collision-free), so an agent can stash REPL output
//! / accumulators without naming an address. The write lands in the per-tree
//! [`TapeStore`](context_tape::TapeStore) marked dirty.
//!
//! Write-back **promotion** into durable memory is doubly gated: it is attempted
//! only when the caller passes `promote=true` AND the daemon's
//! `[tape] allow_promotion` is enabled AND the address is an existing memory
//! observation. The corpus (`file_chunks` / files) is READ-ONLY and is never a
//! promotion target. With promotion off (the default) the bytes live only in the
//! tree store and are discarded on eviction.
//!
//! Boundary: analytical, no shell/exec; never writes the user's source files;
//! the corpus is read-only.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use context_tape::{Page, PageAddress, PageKind as TapePageKind, PageMeta};

use crate::context::SystemContext;
use crate::mcp::server::TapePutParams;
use crate::mcp::tools::sota_helpers::json_result;
use crate::mcp::tools::tape_support::{parse_address, render_address, tree_id_of, tree_path_of};
use crate::tape::data_plane::{TapeDataPlane, TapeError};
use crate::tape::real_data_plane::RealTapeDataPlane;

pub async fn tool_tape_put(
    ctx: &SystemContext,
    params: TapePutParams,
) -> Result<CallToolResult, McpError> {
    let tree_id = tree_id_of(&params.tree);
    let promote = params.promote.unwrap_or(false);

    // Resolve / mint the target address.
    let address = match &params.address {
        Some(a) => parse_address(a)?,
        None => PageAddress::Scratch {
            tree: tree_id,
            // A fresh v4 UUID is a collision-free 16-byte slot.
            slot: uuid::Uuid::new_v4().as_bytes().to_vec().into_boxed_slice(),
        },
    };
    let addr_path = render_address(&address);

    // Promotion path: route through the real data plane so the gated bi-temporal
    // supersession (memory_observations only) runs exactly as P3 specifies. The
    // plane also applies the daemon's `allow_promotion` gate, so a `promote=true`
    // with the daemon flag OFF degrades to a staged-only write (a by-design warn!
    // inside the plane). Only meaningful for an existing observation address.
    if promote
        && matches!(address, PageAddress::Observation { .. })
        && let Some(plane) = RealTapeDataPlane::from_context(ctx)
    {
        let tree_path = tree_path_of(&params.tree);
        let addr = crate::tape::working_set::PageAddr(addr_path.clone());
        match plane.put(&tree_path, &addr, &params.content).await {
            Ok(()) => {
                let allow = ctx.config().load().tape.allow_promotion;
                return json_result(&json!({
                    "tree": params.tree,
                    "address": addr_path,
                    "dirty": true,
                    "promotion_requested": true,
                    // Promotion only actually occurs when the daemon permits it.
                    "promoted": allow,
                }));
            }
            Err(TapeError::Backend(e)) => {
                return Err(McpError::internal_error(
                    format!("tape_put backend error: {e}"),
                    None,
                ));
            }
            Err(TapeError::NotFound(p)) => {
                return Err(McpError::invalid_params(
                    format!("tape_put: address not found for promotion: {p}"),
                    None,
                ));
            }
        }
        // No live pool (else branch): falls through to the staged-only write below.
    }

    // Staged write. When a live pool exists, route through the `PagingEngine` so
    // the page is RESIDENCY-TRACKED in `working_set_pages` (budget-evicted) AND its
    // bytes persist durably (the v53 `content` column) for pause/resume — this is
    // the unification of the in-RAM `TapeStore` and the DB control plane (the C3
    // fix). `admit_scratch` itself stages the bytes into the same per-tree
    // `TapeStore` via the data plane, so the RAM plane is populated identically,
    // plus residency is now tracked. The `session_key` for a tape tree IS its tree
    // path (the bridge: `"rlm:{root_task_id}"` / the verb's tree path).
    let tree_path = tree_path_of(&params.tree);
    let est_tokens = Page::estimate_tokens(&params.content);
    if let Some(pool) = ctx.db().pool().cloned()
        && let Some(plane) = RealTapeDataPlane::from_context(ctx)
    {
        let session_key = tree_path.as_str().to_string();
        let addr = crate::tape::working_set::PageAddr(addr_path.clone());
        let engine = crate::tape::engine::PagingEngine::new(&pool, &plane);
        let mut ws = crate::tape::store::load_working_set(&pool, &session_key, 0)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("tape_put: load working set: {e}"), None)
            })?;
        engine
            .admit_scratch(&mut ws, &tree_path, &addr, &params.content, 0.5)
            .await
            .map_err(|e| McpError::internal_error(format!("tape_put: admit_scratch: {e}"), None))?;
        return json_result(&json!({
            "tree": params.tree,
            "address": addr_path,
            "dirty": true,
            "resident_pages": ws.pages.len(),
            "resident_tokens": ws.resident_tokens,
            "promotion_requested": promote,
            "promoted": false,
        }));
    }

    // No live pool (CLI / mock-DB mode): direct in-RAM stage. Residency is not
    // tracked here (there is no DB to persist it to), which is correct — the
    // control plane is a DB concern.
    ctx.tape_registry().with_store_mut(tree_id, |s| {
        let page = Page::new(
            address.clone(),
            params.content.clone(),
            PageMeta {
                kind: TapePageKind::Scratch,
                est_tokens,
                importance: 0.5,
                dirty: true,
            },
        );
        s.put(address.clone(), page);
    });

    json_result(&json!({
        "tree": params.tree,
        "address": addr_path,
        "dirty": true,
        "promotion_requested": promote,
        "promoted": false,
    }))
}
