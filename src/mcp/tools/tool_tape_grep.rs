//! `tape_grep` — substring search over the tape.
//!
//! - `scope = "tree"` (default): the per-tree store's substring index
//!   ([`AddressIndex::grep`](context_tape::AddressIndex::grep)) over resident
//!   page content — exact, in-RAM, no DB.
//! - `scope = "corpus"`: candidate chunks from the **READ-ONLY** indexed corpus
//!   via [`RealTapeDataPlane::resolve`]`(Grep)` (metadata refs, no bytes
//!   hydrated). Optionally scoped by `project`.
//! - `scope = "both"`: the union of the two, tree hits first.
//!
//! Boundary: analytical, no shell/exec; corpus reads are READ-ONLY; never writes
//! the user's files.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use tracing::warn;

use crate::context::SystemContext;
use crate::mcp::server::TapeGrepParams;
use crate::mcp::tools::sota_helpers::json_result;
use crate::mcp::tools::tape_support::{head_on_boundary, render_address, tree_id_of, tree_path_of};
use crate::tape::data_plane::{PageQuery, TapeDataPlane, TapeError};
use crate::tape::real_data_plane::RealTapeDataPlane;

/// Default cap on grep hits returned.
const DEFAULT_GREP_LIMIT: usize = 64;
/// Per-hit head snippet length for tree-scope matches.
const GREP_SNIPPET_BYTES: usize = 160;

pub async fn tool_tape_grep(
    ctx: &SystemContext,
    params: TapeGrepParams,
) -> Result<CallToolResult, McpError> {
    let scope = params.scope.as_deref().unwrap_or("tree");
    if !matches!(scope, "tree" | "corpus" | "both") {
        return Err(McpError::invalid_params(
            format!("invalid scope '{scope}': expected 'tree', 'corpus', or 'both'"),
            None,
        ));
    }
    let limit = params.limit.unwrap_or(DEFAULT_GREP_LIMIT).max(1);
    let tree_id = tree_id_of(&params.tree);

    let mut hits: Vec<serde_json::Value> = Vec::with_capacity(limit);

    // --- tree scope: the per-tree substring index over resident content. ---
    if matches!(scope, "tree" | "both") {
        ctx.tape_registry().with_store(tree_id, |s| {
            for addr in s.index().grep(&params.pattern) {
                if hits.len() >= limit {
                    break;
                }
                let snippet = s.get(&addr).map(|p| {
                    let (h, truncated) = head_on_boundary(&p.content, GREP_SNIPPET_BYTES);
                    (h.to_string(), truncated)
                });
                let (snip, trunc) = snippet.unwrap_or_default();
                hits.push(json!({
                    "address": render_address(&addr),
                    "scope": "tree",
                    "snippet": snip,
                    "snippet_truncated": trunc,
                }));
            }
        });
    }

    // --- corpus scope: READ-ONLY chunk candidates via the real data plane. ---
    if matches!(scope, "corpus" | "both") && hits.len() < limit {
        match RealTapeDataPlane::from_context(ctx) {
            Some(plane) => {
                let tree_path = tree_path_of(&params.tree);
                let query = PageQuery::Grep {
                    pattern: params.pattern.clone(),
                };
                match plane.resolve(&tree_path, &query).await {
                    Ok(refs) => {
                        for r in refs {
                            if hits.len() >= limit {
                                break;
                            }
                            hits.push(json!({
                                "address": r.addr.0,
                                "scope": "corpus",
                                "kind": r.kind.as_str(),
                                "est_tokens": r.est_tokens,
                                "importance": r.importance,
                            }));
                        }
                    }
                    Err(TapeError::Backend(e)) => {
                        return Err(McpError::internal_error(
                            format!("tape_grep corpus backend error: {e}"),
                            None,
                        ));
                    }
                    Err(TapeError::NotFound(_)) => { /* benign: no corpus matches */ }
                }
            }
            None => {
                // CLI / mock-DB mode: corpus grep is unavailable. By-design benign
                // (the tree-scope results, if any, still returned). ADR-021 warn!.
                warn!(
                    scope = scope,
                    "tape_grep: corpus scope unavailable without a live PgPool; returning tree-scope hits only"
                );
            }
        }
    }

    let _ = &params.project; // project scoping is advisory for corpus grep today
    json_result(&json!({
        "tree": params.tree,
        "pattern": params.pattern,
        "scope": scope,
        "hits": hits,
    }))
}
