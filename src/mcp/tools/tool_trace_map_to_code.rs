//! `tool_trace_map_to_code` (Dbg-2) — map an agent-provided backtrace to
//! `file:line` + symbol per frame, with any attached memory-graph entities.
//!
//! Given a stack trace (gdb `bt`, a BCC off-CPU folded stack, or a plain
//! newline/`;`-separated frame list), resolve each application frame to its
//! `file_symbols` row (file + line) and surface the memory-graph entities
//! anchored to that symbol/file (`memory_find_entities_for_code`). This turns an
//! opaque trace into clickable code locations enriched with prior knowledge.
//!
//! Read-only: pgmcp parses the agent's text and runs SELECTs (symbol resolution
//! plus memory anchors). Frame→symbol matching is by bare name, the same proxy
//! the profile/deadlock bridges use.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::TraceMapToCodeParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_trace_map_to_code(
    ctx: &SystemContext,
    params: TraceMapToCodeParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    let project = params.project.trim();
    if project.is_empty() {
        return Err(McpError::invalid_params("project must be non-empty", None));
    }
    let format = params.format.as_deref().unwrap_or("auto").trim();
    if !matches!(format, "gdb_bt" | "folded" | "auto") {
        return Err(McpError::invalid_params(
            format!("unknown format '{format}'; expected gdb_bt | folded | auto"),
            None,
        ));
    }
    let limit = params.limit.unwrap_or(64).clamp(1, 512) as usize;

    debug!(
        tool = "trace_map_to_code",
        project, format, "MCP tool invoked"
    );

    let pool = pool_or_err(ctx)?;
    let project_id = project_id_or_err(ctx, project).await?;

    // Parse the backtrace into an ordered list of (frame_index, bare_symbol).
    let frames = parse_frames(format, &params.backtrace, limit);
    if frames.is_empty() {
        return json_result(&json!({
            "project": project,
            "format": format,
            "frames_parsed": 0,
            "frames": [],
            "guidance": "No frames parsed. Confirm the format (gdb `#N` frames / `;`-folded stack / \
                         newline-separated symbol list).",
        }));
    }

    // Resolve all distinct bare names in one query.
    let names: Vec<String> = {
        let mut seen: HashSet<String> = HashSet::new();
        frames
            .iter()
            .filter_map(|(_, n)| {
                if seen.insert(n.clone()) {
                    Some(n.clone())
                } else {
                    None
                }
            })
            .collect()
    };
    let rows = queries::resolve_profile_symbols(pool, project_id, &names)
        .await
        .map_err(|e| McpError::internal_error(format!("symbol resolution failed: {e}"), None))?;

    // name → best (highest-pagerank) resolved row (query is pre-ordered).
    let mut resolved_by_name: std::collections::HashMap<
        String,
        &queries::ResolvedProfileSymbolRow,
    > = std::collections::HashMap::new();
    for r in &rows {
        resolved_by_name.entry(r.name.clone()).or_insert(r);
    }

    let mut frames_json: Vec<serde_json::Value> = Vec::with_capacity(frames.len());
    let mut resolved_count = 0usize;
    for (idx, name) in &frames {
        match resolved_by_name.get(name) {
            Some(r) => {
                resolved_count += 1;
                // Attach memory-graph entities anchored to this symbol.
                let anchors = queries::memory_find_entities_for_code(
                    pool,
                    None,
                    None,
                    None,
                    Some(r.symbol_id),
                    None,
                )
                .await
                .unwrap_or_default();
                let entity_ids: Vec<i64> = anchors.iter().map(|a| a.entity_id).collect();
                frames_json.push(json!({
                    "frame": idx,
                    "symbol": name,
                    "resolved": true,
                    "file": r.relative_path,
                    "language": r.language,
                    "line": r.start_line,
                    "symbol_id": r.symbol_id,
                    "memory_entity_ids": entity_ids,
                    "memory_entity_count": entity_ids.len(),
                }));
            }
            None => {
                frames_json.push(json!({
                    "frame": idx,
                    "symbol": name,
                    "resolved": false,
                }));
            }
        }
    }

    let result = json!({
        "project": project,
        "format": format,
        "frames_parsed": frames.len(),
        "resolved_count": resolved_count,
        "unresolved_count": frames.len() - resolved_count,
        "frames": frames_json,
        "guidance": "Each backtrace frame mapped to file:line via exact symbol-name resolution, with \
                     memory-graph entities anchored to the symbol. Unresolved frames are external \
                     (libc / kernel / inlined) or not indexed — run `symbol-extraction` for fuller \
                     coverage, or `fuzzy_symbol_search` to hunt a near-name match.",
    });

    debug!(
        tool = "trace_map_to_code",
        frames = frames.len(),
        resolved = resolved_count,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    json_result(&result)
}

/// Parse the backtrace into `(frame_index, bare_symbol)` pairs, leaf first.
fn parse_frames(format: &str, text: &str, limit: usize) -> Vec<(usize, String)> {
    let chosen = if format == "auto" {
        sniff_format(text)
    } else {
        format
    };
    let mut out: Vec<(usize, String)> = Vec::new();
    match chosen {
        "gdb_bt" => {
            for line in text.lines() {
                let l = line.trim();
                if let Some(rest) = l.strip_prefix('#')
                    && let Some(sym) = gdb_frame_symbol(rest)
                {
                    out.push((out.len(), sym));
                    if out.len() >= limit {
                        break;
                    }
                }
            }
        }
        "folded" => {
            // Take the first (deepest-count) folded line; frames are leaf-LAST,
            // so reverse to present leaf-first.
            if let Some(first) = text.lines().map(str::trim).find(|l| !l.is_empty()) {
                let stack = first
                    .rsplit_once(char::is_whitespace)
                    .filter(|(_, c)| c.trim().parse::<u64>().is_ok())
                    .map(|(s, _)| s)
                    .unwrap_or(first);
                for frame in stack.split(';').rev() {
                    if let Some(sym) = clean_symbol(frame) {
                        out.push((out.len(), sym));
                        if out.len() >= limit {
                            break;
                        }
                    }
                }
            }
        }
        _ => {
            // Plain newline/`;`-separated symbol list.
            let parts: Vec<&str> = if text.contains(';') {
                text.split(';').collect()
            } else {
                text.lines().collect()
            };
            for p in parts {
                if let Some(sym) = clean_symbol(p) {
                    out.push((out.len(), sym));
                    if out.len() >= limit {
                        break;
                    }
                }
            }
        }
    }
    out
}

/// Heuristically choose a parser when `format = auto`.
fn sniff_format(text: &str) -> &'static str {
    let has_gdb = text.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with('#') && t[1..].chars().next().is_some_and(|c| c.is_ascii_digit())
    });
    if has_gdb {
        return "gdb_bt";
    }
    // A folded stack is a single line with `;` separators and a trailing count.
    let folded_like = text.lines().any(|l| {
        let l = l.trim();
        l.contains(';')
            && l.rsplit_once(char::is_whitespace)
                .is_some_and(|(_, c)| c.trim().parse::<u64>().is_ok())
    });
    if folded_like {
        return "folded";
    }
    "list"
}

/// Extract the symbol from a gdb frame body (the part after `#`).
fn gdb_frame_symbol(rest: &str) -> Option<String> {
    let after_num = rest
        .trim_start()
        .split_once(char::is_whitespace)
        .map(|(_, r)| r)
        .unwrap_or(rest)
        .trim();
    let candidate = if let Some(idx) = after_num.find(" in ") {
        &after_num[idx + 4..]
    } else {
        after_num
    };
    clean_symbol(candidate)
}

/// Reduce a frame token to its bare final-segment symbol, dropping addresses,
/// `+0x` offsets, ` (dso)` / ` (args)` annotations, and path/generic qualifiers.
/// Returns `None` for empty or pure-address frames.
fn clean_symbol(frame: &str) -> Option<String> {
    let f = frame.trim();
    let f = f.split(" (").next().unwrap_or(f);
    let f = f.split(" at ").next().unwrap_or(f);
    let f = f.split("+0x").next().unwrap_or(f).trim();
    if f.is_empty() || f.starts_with("0x") {
        return None;
    }
    let after_path = f.rsplit("::").next().unwrap_or(f);
    let bare = after_path
        .split(['<', '(', ' ', '@'])
        .next()
        .unwrap_or(after_path)
        .trim();
    if bare.is_empty() || bare.starts_with("0x") {
        return None;
    }
    Some(bare.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gdb_frames() {
        let text = "\
#0  0x00007f in pthread_mutex_lock () from /lib/libpthread.so
#1  0x000055 in db::lock_resource (self=0x1) at src/db.rs:42
#2  0x000055 in handle_txn () at src/db.rs:90
";
        let frames = parse_frames("auto", text, 64);
        assert_eq!(frames.len(), 3, "frames: {:?}", frames);
        assert_eq!(frames[0].1, "pthread_mutex_lock");
        assert_eq!(frames[1].1, "lock_resource");
        assert_eq!(frames[2].1, "handle_txn");
    }

    #[test]
    fn parse_folded_frames_leaf_first() {
        let text = "main;worker;handle;acquire 42\n";
        let frames = parse_frames("auto", text, 64);
        // leaf-first: acquire, handle, worker, main
        assert_eq!(frames[0].1, "acquire");
        assert_eq!(frames.last().expect("last").1, "main");
    }

    #[test]
    fn parse_plain_list() {
        let text = "foo::bar\nbaz::qux\n";
        let frames = parse_frames("list", text, 64);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].1, "bar");
        assert_eq!(frames[1].1, "qux");
    }

    #[test]
    fn sniff_detects_gdb_and_folded() {
        assert_eq!(sniff_format("#0  0x1 in f ()\n"), "gdb_bt");
        assert_eq!(sniff_format("a;b;c 10\n"), "folded");
        assert_eq!(sniff_format("plain_name\nanother\n"), "list");
    }

    #[test]
    fn clean_symbol_strips_decorations() {
        assert_eq!(clean_symbol("myapp::db::lock_x").as_deref(), Some("lock_x"));
        assert_eq!(clean_symbol("func+0x1a (libc.so)").as_deref(), Some("func"));
        assert_eq!(clean_symbol("Foo<T>::method").as_deref(), Some("method"));
        assert_eq!(clean_symbol("0xdeadbeef"), None);
        assert_eq!(clean_symbol("   "), None);
    }

    #[test]
    fn gdb_frame_symbol_extracts_after_in() {
        assert_eq!(
            gdb_frame_symbol("0  0x00007f in pthread_mutex_lock ()").as_deref(),
            Some("pthread_mutex_lock")
        );
        assert_eq!(
            gdb_frame_symbol("2  my_func (x=1) at f.c:10").as_deref(),
            Some("my_func")
        );
    }

    #[test]
    fn limit_caps_frames() {
        let text = "a\nb\nc\nd\ne\n";
        let frames = parse_frames("list", text, 3);
        assert_eq!(frames.len(), 3);
    }
}
