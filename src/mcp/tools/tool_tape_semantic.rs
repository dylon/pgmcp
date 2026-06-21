//! `tape_semantic` — top-`k` semantic retrieval over the **READ-ONLY** corpus.
//!
//! Embeds the natural-language `query` host-side ([`crate::embed::EmbedSource`])
//! and runs the k-NN through [`RealTapeDataPlane::resolve`]`(Semantic)`. The
//! returned references are metadata-only (no bytes hydrated); each hit's
//! `similarity` is the cosine score the ranker used. Requires a live PgPool; in
//! CLI / mock-DB mode the semantic path is unavailable and an empty hit list is
//! returned (a by-design, benign no-op).
//!
//! Boundary: analytical, no shell/exec; corpus reads are READ-ONLY; never writes
//! the user's files.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use tracing::warn;

use crate::context::SystemContext;
use crate::mcp::server::TapeSemanticParams;
use crate::mcp::tools::sota_helpers::json_result;
use crate::mcp::tools::tape_support::tree_path_of;
use crate::tape::data_plane::{PageQuery, TapeDataPlane, TapeError};
use crate::tape::real_data_plane::RealTapeDataPlane;

/// Default number of nearest hits.
const DEFAULT_K: usize = 8;

pub async fn tool_tape_semantic(
    ctx: &SystemContext,
    params: TapeSemanticParams,
) -> Result<CallToolResult, McpError> {
    let k = params.k.unwrap_or(DEFAULT_K).max(1);

    let Some(plane) = RealTapeDataPlane::from_context(ctx) else {
        // CLI / mock-DB mode: the embedding + corpus retrieval path needs a live
        // pool. By-design benign no-op (ADR-021 warn!).
        warn!(
            "tape_semantic: unavailable without a live PgPool (CLI/mock-DB mode); returning no hits"
        );
        return json_result(&json!({
            "tree": params.tree,
            "query": params.query,
            "k": k,
            "hits": [],
            "available": false,
        }));
    };

    let tree_path = tree_path_of(&params.tree);
    let query = PageQuery::Semantic {
        query: params.query.clone(),
        k,
    };
    match plane.resolve(&tree_path, &query).await {
        Ok(refs) => {
            let mut hits = Vec::with_capacity(refs.len());
            for r in refs {
                hits.push(json!({
                    "address": r.addr.0,
                    // resolve(Semantic) maps the cosine score onto `importance`.
                    "similarity": r.importance,
                    "kind": r.kind.as_str(),
                    "est_tokens": r.est_tokens,
                }));
            }
            let _ = &params.project; // corpus-wide retrieval; project scoping advisory
            json_result(&json!({
                "tree": params.tree,
                "query": params.query,
                "k": k,
                "hits": hits,
                "available": true,
            }))
        }
        Err(TapeError::Backend(e)) => Err(McpError::internal_error(
            format!("tape_semantic backend error: {e}"),
            None,
        )),
        Err(TapeError::NotFound(_)) => json_result(&json!({
            "tree": params.tree,
            "query": params.query,
            "k": k,
            "hits": [],
            "available": true,
        })),
    }
}
