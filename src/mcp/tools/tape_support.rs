//! Shared scaffolding for the **tape verb** tool bodies (Phase 4).
//!
//! The tape verbs share a small amount of plumbing: derive the per-tree
//! [`TreeId`](context_tape::TreeId) from the agent-supplied `tree` string,
//! translate the MCP `address` string ↔ [`context_tape::PageAddress`] through
//! the P3 bridge, and shape error responses. This module centralizes that so
//! each `tool_tape_*` file stays focused on its one verb.
//!
//! ## Trust boundary (applies to every verb)
//!
//! These verbs are **analytical and black-box-legal**: they never run a shell,
//! never execute agent-supplied code, and never write the user's source files.
//! Reads may hydrate from the durable corpus (strictly READ-ONLY); writes target
//! the per-tree [`TapeStore`](context_tape::TapeStore) via the
//! [`TapeRegistry`](crate::tape::registry::TapeRegistry). The corpus tables are
//! never mutated by any tape verb.

use rmcp::ErrorData as McpError;

use context_tape::{PageAddress, TreeId};

use crate::tape::data_plane::TreePath;
use crate::tape::real_data_plane::RealTapeDataPlane;

/// Canonical [`TreePath`] for an agent-supplied `tree` string. Mirrors the P5
/// orchestrator's `RlmFrame.root_task_id → TreePath` derivation exactly, so a
/// verb addresses the *same* per-tree store the paging engine populates.
#[inline]
pub fn tree_path_of(tree: &str) -> TreePath {
    TreePath::for_root_task(tree)
}

/// Derive the per-tree [`TreeId`] for an agent-supplied `tree` string. Routes
/// through the SOLE authority [`RealTapeDataPlane::tree_id`] so the id matches
/// the one P3/P5 derive (deterministic SHA-256 of the tree path).
#[inline]
pub fn tree_id_of(tree: &str) -> TreeId {
    RealTapeDataPlane::tree_id(&tree_path_of(tree))
}

/// Parse an MCP `address` string into a typed [`PageAddress`]. The string IS
/// `PageAddress::to_path()` (the P3 invariant), so this is the bridge's
/// `parse_path`. A malformed address is a caller error (`invalid_params`),
/// never a panic.
pub fn parse_address(address: &str) -> Result<PageAddress, McpError> {
    PageAddress::parse_path(address).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "malformed page address '{address}': expected a data-plane path such as \
                 'corpus/chunk/<id>', 'corpus/file/<id>', 'corpus/file/<id>/region/<lo>..<hi>', \
                 'memory/obs/<id>', or 'scratch/<tree-uuid>/<hex-slot>'"
            ),
            None,
        )
    })
}

/// Render a typed [`PageAddress`] back to its canonical path string (total).
#[inline]
pub fn render_address(address: &PageAddress) -> String {
    address.to_path()
}

/// Truncate `s` to at most `max` bytes on a UTF-8 char boundary (so the head
/// preview never splits a multibyte codepoint). Returns the (possibly
/// truncated) prefix plus whether truncation occurred.
pub fn head_on_boundary(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    // Walk back to the nearest char boundary at or below `max`.
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_id_is_stable_and_matches_authority() {
        // The verb derivation must equal the SOLE authority's, so a verb and
        // the paging engine address the same per-tree store.
        let direct = RealTapeDataPlane::tree_id(&TreePath::for_root_task("t-1"));
        assert_eq!(tree_id_of("t-1"), direct);
        assert_eq!(tree_id_of("t-1"), tree_id_of("t-1"));
        assert_ne!(tree_id_of("t-1"), tree_id_of("t-2"));
    }

    #[test]
    fn address_round_trips_through_the_bridge() {
        for path in [
            "corpus/chunk/42",
            "corpus/file/5",
            "corpus/file/5/region/2..9",
            "memory/obs/7",
        ] {
            let addr = parse_address(path).expect("legal path parses");
            assert_eq!(render_address(&addr), path);
        }
    }

    #[test]
    fn malformed_address_is_invalid_params_not_panic() {
        assert!(parse_address("nonsense").is_err());
        assert!(parse_address("corpus/chunk/not-a-number").is_err());
        assert!(parse_address("").is_err());
    }

    #[test]
    fn head_respects_char_boundaries() {
        let (h, trunc) = head_on_boundary("abcdef", 3);
        assert_eq!(h, "abc");
        assert!(trunc);
        let (h, trunc) = head_on_boundary("abc", 10);
        assert_eq!(h, "abc");
        assert!(!trunc);
        // A 2-byte 'é' at the cut point must not be split.
        let s = "aé"; // 'a' (1) + 'é' (2) = 3 bytes
        let (h, trunc) = head_on_boundary(s, 2);
        assert_eq!(h, "a", "must not split the multibyte char");
        assert!(trunc);
    }
}
