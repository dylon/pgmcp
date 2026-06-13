//! Shared result-slimming helpers for the high-frequency search tools.
//!
//! The central re-encoder (`server::reencode_result_for_format`) handles the
//! *wire format* (compact JSON for token-sensitive clients). This module handles
//! *payload content*: truncating the unbounded `chunk_content` to a snippet and
//! projecting / eliding fields, so a 10-hit search does not ship 10×1–4 KB of
//! full chunk bodies when a preview suffices. Applied in the semantic / text /
//! grep tool bodies on the result envelope before serialization.

use serde_json::Value;

use crate::mcp::client_profile::RenderCtx;

/// Default snippet length (chars of `chunk_content`) applied to `default_brief`
/// clients when they do not pass an explicit `snippet_length`. Rich clients
/// (claude-code) keep full content.
pub const DEFAULT_BRIEF_SNIPPET: usize = 500;

/// The content-field names search tools use across their result shapes.
const CONTENT_FIELDS: [&str; 3] = ["chunk_content", "content", "snippet"];
/// Fields that are redundant for a brief client (derivable from `path` / request
/// context) and dropped from `default_brief` output unless an explicit `fields`
/// projection is given.
const BRIEF_DROP_FIELDS: [&str; 2] = ["relative_path", "project_name"];

/// Char-safe truncation to `max_chars` with a trailing ellipsis. Counts by
/// Unicode scalar so we never split a multi-byte char.
pub fn truncate_chunk(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// Slim the `results` array of a search envelope in place:
///
/// - `snippet_len` (or, for `default_brief` clients, [`DEFAULT_BRIEF_SNIPPET`])
///   truncates each hit's content field to a preview.
/// - `fields`, when given, projects each hit to exactly those keys; otherwise a
///   `default_brief` client drops the redundant `relative_path` / `project_name`.
///
/// A no-op for rich clients (claude-code) that pass neither `snippet_len` nor
/// `fields`, so their output stays byte-identical.
pub fn shape_search_results(
    envelope: &mut Value,
    snippet_len: Option<usize>,
    fields: Option<&[String]>,
    rc: RenderCtx,
) {
    // Search tools name their hit array either "results" (semantic/text) or
    // "hits" (grep).
    let key = if envelope.get("results").is_some() {
        "results"
    } else {
        "hits"
    };
    let Some(arr) = envelope.get_mut(key).and_then(|v| v.as_array_mut()) else {
        return;
    };

    let effective_snippet = snippet_len.or(if rc.default_brief {
        Some(DEFAULT_BRIEF_SNIPPET)
    } else {
        None
    });

    for item in arr.iter_mut() {
        let Some(obj) = item.as_object_mut() else {
            continue;
        };

        if let Some(max) = effective_snippet {
            for key in CONTENT_FIELDS {
                let truncated = match obj.get(key) {
                    Some(Value::String(s)) if s.chars().count() > max => {
                        Some(truncate_chunk(s, max))
                    }
                    _ => None,
                };
                if let Some(t) = truncated {
                    obj.insert(key.to_string(), Value::String(t));
                }
            }
        }

        match fields {
            Some(keep) => {
                obj.retain(|k, _| keep.iter().any(|f| f == k));
            }
            None if rc.default_brief => {
                for drop in BRIEF_DROP_FIELDS {
                    obj.remove(drop);
                }
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::client_profile::OutputFormat;
    use serde_json::json;

    fn brief() -> RenderCtx {
        RenderCtx {
            output_format: OutputFormat::CompactJson,
            default_brief: true,
            include_provenance: false,
        }
    }

    #[test]
    fn truncate_is_char_safe_and_adds_ellipsis() {
        assert_eq!(truncate_chunk("hello", 10), "hello");
        assert_eq!(truncate_chunk("hello world", 5), "hello…");
        // Multi-byte: never panics on a char boundary.
        let s = "αβγδεζηθ";
        let out = truncate_chunk(s, 3);
        assert_eq!(out, "αβγ…");
    }

    #[test]
    fn brief_truncates_content_and_drops_redundant_fields() {
        let mut env = json!({
            "results": [{
                "path": "/abs/x.rs",
                "relative_path": "x.rs",
                "project_name": "p",
                "chunk_content": "x".repeat(1000),
                "score": 0.9,
            }]
        });
        shape_search_results(&mut env, None, None, brief());
        let hit = &env["results"][0];
        assert!(hit.get("relative_path").is_none());
        assert!(hit.get("project_name").is_none());
        assert_eq!(
            hit["chunk_content"].as_str().unwrap().chars().count(),
            DEFAULT_BRIEF_SNIPPET + 1 // + ellipsis
        );
        assert!(hit.get("path").is_some()); // kept
    }

    #[test]
    fn explicit_fields_projects_exactly() {
        let mut env = json!({
            "results": [{"path": "a", "score": 0.9, "chunk_content": "body"}]
        });
        shape_search_results(
            &mut env,
            None,
            Some(&["path".to_string(), "score".to_string()]),
            RenderCtx::default(),
        );
        let hit = env["results"][0].as_object().unwrap();
        assert_eq!(hit.len(), 2);
        assert!(hit.contains_key("path") && hit.contains_key("score"));
    }

    #[test]
    fn rich_client_is_noop() {
        let original = json!({
            "results": [{"path": "a", "relative_path": "a", "project_name": "p",
                         "chunk_content": "x".repeat(1000)}]
        });
        let mut env = original.clone();
        shape_search_results(&mut env, None, None, RenderCtx::default());
        assert_eq!(env, original, "rich client output must be byte-identical");
    }
}
