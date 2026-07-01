//! `tape_excerpt` — fetch a bounded excerpt from one tape page.
//!
//! This is the safe materialization companion to `tape_get`: it uses the same
//! hot/out-of-core/corpus-hydrate cascade to locate a page, but it never returns
//! more than a small hard-capped slice to the caller. That makes it suitable for
//! black-box agents whose live transcript must not accidentally hydrate a whole
//! large page.
//!
//! Boundary: analytical, no shell/exec; reads only (hydrate is READ-ONLY); never
//! writes the user's files.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::TapeExcerptParams;
use crate::mcp::tools::sota_helpers::json_result;
use crate::mcp::tools::tape_support::{
    head_on_boundary, parse_address, render_address, tree_id_of, tree_path_of,
};
use crate::tape::data_plane::{TapeDataPlane, TapeError};
use crate::tape::real_data_plane::RealTapeDataPlane;

const DEFAULT_EXCERPT_BYTES: usize = 4 * 1024;
const HARD_EXCERPT_BYTES: usize = 8 * 1024;

pub async fn tool_tape_excerpt(
    ctx: &SystemContext,
    params: TapeExcerptParams,
) -> Result<CallToolResult, McpError> {
    let address = parse_address(&params.address)?;
    let tree_path = tree_path_of(&params.tree);
    let tree_id = tree_id_of(&params.tree);
    let addr_path = render_address(&address);
    let max_bytes = params
        .max_bytes
        .unwrap_or(DEFAULT_EXCERPT_BYTES)
        .clamp(1, HARD_EXCERPT_BYTES);

    if params.start_byte.is_some() && (params.start_line.is_some() || params.end_line.is_some()) {
        return Err(McpError::invalid_params(
            "tape_excerpt accepts either start_byte or start_line/end_line, not both",
            None,
        ));
    }

    let dirty = ctx
        .tape_registry()
        .with_store(tree_id, |s| s.is_dirty(&address));

    let loaded = match RealTapeDataPlane::from_context(ctx) {
        Some(plane) => {
            let addr = crate::tape::working_set::PageAddr(addr_path.clone());
            match plane.get(&tree_path, &addr).await {
                Ok(content) => Some((content.bytes, i64::from(content.est_tokens))),
                Err(TapeError::NotFound(p)) => {
                    return json_result(&json!({
                        "tree": params.tree,
                        "address": addr_path,
                        "found": false,
                        "reason": format!("page not resident and not hydratable: {p}"),
                    }));
                }
                Err(TapeError::Backend(e)) => {
                    return Err(McpError::internal_error(
                        format!("tape_excerpt backend error: {e}"),
                        None,
                    ));
                }
            }
        }
        None => ctx.tape_registry().with_store(tree_id, |s| {
            s.get_cascade(&address)
                .map(|p| (p.content, i64::from(p.meta.est_tokens)))
        }),
    };

    let Some((content, page_est_tokens)) = loaded else {
        return json_result(&json!({
            "tree": params.tree,
            "address": addr_path,
            "found": false,
            "reason": "page not resident (corpus hydrate unavailable in this mode)",
        }));
    };

    let selection = select_range(&content, &params)?;
    let source = &content[selection.start_byte..selection.end_byte];
    let (excerpt, byte_truncated) = head_on_boundary(source, max_bytes);
    let end_byte = selection.start_byte + excerpt.len();

    json_result(&json!({
        "tree": params.tree,
        "address": addr_path,
        "found": true,
        "content": excerpt,
        "start_byte": selection.start_byte,
        "end_byte": end_byte,
        "size_bytes": content.len(),
        "excerpt_bytes": excerpt.len(),
        "est_tokens": estimate_tokens(excerpt),
        "page_est_tokens": page_est_tokens,
        "max_bytes": max_bytes,
        "content_truncated": end_byte < selection.end_byte || byte_truncated,
        "dirty": dirty,
        "start_line": selection.start_line,
        "end_line": selection.end_line,
    }))
}

#[derive(Debug, Clone, Copy)]
struct Selection {
    start_byte: usize,
    end_byte: usize,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

fn select_range(content: &str, params: &TapeExcerptParams) -> Result<Selection, McpError> {
    if params.start_line.is_some() || params.end_line.is_some() {
        let start_line = params.start_line.unwrap_or(1);
        let end_line = params.end_line.unwrap_or(usize::MAX);
        if start_line == 0 || end_line == 0 {
            return Err(McpError::invalid_params(
                "tape_excerpt line numbers are 1-based and must be positive",
                None,
            ));
        }
        let (start_byte, end_byte, actual_start, actual_end) =
            line_byte_range(content, start_line, end_line);
        return Ok(Selection {
            start_byte,
            end_byte,
            start_line: Some(actual_start),
            end_line: Some(actual_end),
        });
    }

    let start_byte = next_char_boundary(content, params.start_byte.unwrap_or(0));
    Ok(Selection {
        start_byte,
        end_byte: content.len(),
        start_line: None,
        end_line: None,
    })
}

fn line_byte_range(
    content: &str,
    start_line: usize,
    end_line: usize,
) -> (usize, usize, usize, usize) {
    if content.is_empty() || start_line > end_line {
        return (0, 0, start_line, start_line.saturating_sub(1));
    }

    let mut line = 1usize;
    let mut start_byte = content.len();
    let mut found_start = false;
    let mut offset = 0usize;

    for segment in content.split_inclusive('\n') {
        let segment_start = offset;
        let segment_end = offset + segment.len();
        if line == start_line {
            start_byte = segment_start;
            found_start = true;
        }
        if line == end_line {
            return (start_byte, segment_end, start_line, end_line);
        }
        offset = segment_end;
        line += 1;
    }

    if !found_start {
        (
            content.len(),
            content.len(),
            start_line,
            start_line.saturating_sub(1),
        )
    } else {
        (
            start_byte,
            content.len(),
            start_line,
            line.saturating_sub(1),
        )
    }
}

fn next_char_boundary(content: &str, requested: usize) -> usize {
    let mut idx = requested.min(content.len());
    while idx < content.len() && !content.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> TapeExcerptParams {
        TapeExcerptParams {
            tree: "t".into(),
            address: "scratch/00000000-0000-0000-0000-000000000000/01".into(),
            start_line: None,
            end_line: None,
            start_byte: None,
            max_bytes: None,
        }
    }

    #[test]
    fn line_selection_is_one_based_and_inclusive() {
        let mut p = params();
        p.start_line = Some(2);
        p.end_line = Some(3);
        let s = "a\nbb\nccc\ndddd";
        let sel = select_range(s, &p).expect("valid line range");
        assert_eq!(&s[sel.start_byte..sel.end_byte], "bb\nccc\n");
        assert_eq!(sel.start_line, Some(2));
        assert_eq!(sel.end_line, Some(3));
    }

    #[test]
    fn start_byte_moves_forward_to_a_char_boundary() {
        let mut p = params();
        p.start_byte = Some(2);
        let s = "aéz";
        let sel = select_range(s, &p).expect("valid byte range");
        assert_eq!(&s[sel.start_byte..], "z");
    }

    #[test]
    fn empty_or_reversed_line_ranges_are_empty() {
        let mut p = params();
        p.start_line = Some(4);
        p.end_line = Some(2);
        let sel = select_range("a\nb", &p).expect("empty range is legal");
        assert_eq!(sel.start_byte, 0);
        assert_eq!(sel.end_byte, 0);
    }
}
