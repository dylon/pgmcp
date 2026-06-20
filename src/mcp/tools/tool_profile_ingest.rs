//! `tool_profile_ingest` — bridge a profiler's hot symbols to the static code
//! graph (Opt-2).
//!
//! Takes an agent-provided profile artifact (perf report, folded/collapsed
//! flamegraph stacks, or a massif dump), parses the hot symbols/frames, and
//! resolves each to its `file_symbols` row (file + line) joined with the
//! function-level PageRank and complexity metrics pgmcp already maintains. The
//! intersection of "hot at runtime" × "central in the call graph" × "complex /
//! panic-heavy" is exactly where optimization and hardening effort pays off.
//!
//! Modeled on `tool_hot_path_audit`. Structurally read-only: it parses the
//! agent's text and runs SELECTs. pgmcp never runs perf/valgrind itself.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use tracing::{debug, error};

use crate::context::SystemContext;
use crate::db::queries;
use crate::experiment::extract;
use crate::mcp::server::ProfileIngestParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

/// One hot symbol parsed from the profile, before code-graph resolution.
struct HotSymbol {
    /// The raw symbol text as it appeared in the profile.
    raw: String,
    /// A profile-kind-appropriate intensity score (self % for perf, sample
    /// count for flamegraph, bytes for massif).
    intensity: f64,
    /// Human label for the intensity unit.
    unit: &'static str,
}

pub async fn tool_profile_ingest(
    ctx: &SystemContext,
    params: ProfileIngestParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    let project = params.project.trim();
    if project.is_empty() {
        return Err(McpError::invalid_params("project must be non-empty", None));
    }
    let kind = params.kind.trim();
    if !matches!(kind, "perf" | "flamegraph" | "massif") {
        return Err(McpError::invalid_params(
            format!("unknown kind '{kind}'; expected perf | flamegraph | massif"),
            None,
        ));
    }
    let limit = params.limit.unwrap_or(25).clamp(1, 200) as usize;

    debug!(
        tool = "profile_ingest",
        project, kind, limit, "MCP tool invoked"
    );

    let pool = pool_or_err(ctx)?;
    let project_id = project_id_or_err(ctx, project).await?;

    // Parse the artifact into ranked hot symbols (pure text → structs).
    let hot = parse_hot_symbols(kind, &params.content, limit);
    if hot.is_empty() {
        return json_result(&json!({
            "project": project,
            "kind": kind,
            "hot_symbols_parsed": 0,
            "resolved": [],
            "unresolved": [],
            "guidance": "No hot symbols parsed from the artifact. Confirm the format matches \
                         `kind` (perf stdio table / folded stacks / massif dump).",
            "health": health_envelope(false, false),
        }));
    }

    // Build the bare-name set for resolution (a profile symbol like
    // `myapp::index::build` or `_ZN5myapp5build17h..E` resolves on its final
    // identifier segment). We keep a multimap from bare name → the parsed hot
    // entries so several mangled forms collapse onto one query name.
    let mut by_bare: HashMap<String, Vec<&HotSymbol>> = HashMap::new();
    for h in &hot {
        let bare = bare_symbol_name(&h.raw);
        if bare.is_empty() {
            continue;
        }
        by_bare.entry(bare).or_default().push(h);
    }
    let names: Vec<String> = by_bare.keys().cloned().collect();

    let rows = queries::resolve_profile_symbols(pool, project_id, &names)
        .await
        .map_err(|e| {
            // ADR-021: a swallowed/degraded DB failure logs at error!.
            error!(project, kind, error = %e, "profile_ingest: symbol resolution query failed");
            McpError::internal_error(format!("Symbol resolution failed: {e}"), None)
        })?;

    // Index resolved rows by name; keep the highest-PageRank match per name
    // (the query already orders by pagerank desc, cyclomatic desc).
    let mut resolved_by_name: HashMap<String, &queries::ResolvedProfileSymbolRow> = HashMap::new();
    for r in &rows {
        resolved_by_name.entry(r.name.clone()).or_insert(r);
    }

    let graph_present = rows.iter().any(|r| r.pagerank.is_some_and(|p| p > 0.0));
    let metrics_present = rows.iter().any(|r| r.cyclomatic.is_some());

    let mut resolved: Vec<serde_json::Value> = Vec::new();
    let mut unresolved: Vec<serde_json::Value> = Vec::new();

    for h in &hot {
        let bare = bare_symbol_name(&h.raw);
        match resolved_by_name.get(&bare) {
            Some(r) => {
                let pagerank = r.pagerank.unwrap_or(0.0);
                let file_pagerank = r.file_pagerank.unwrap_or(0.0);
                let cyclomatic = r.cyclomatic.unwrap_or(0);
                let panic_paths = r.panic_paths.unwrap_or(0);
                // Priority = runtime intensity weighted by static centrality and
                // complexity. A hot, central, complex, panic-heavy function is
                // the top refactor/optimize target.
                let centrality = pagerank.max(file_pagerank);
                let priority = h.intensity
                    * (1.0 + centrality)
                    * (1.0 + cyclomatic as f64 / 10.0)
                    * (1.0 + panic_paths as f64 / 5.0);

                // Actionable recommendation, first-match-wins.
                let (action, rationale) = if panic_paths >= 3 {
                    (
                        "audit allocations / panic paths",
                        "Hot + many panic/unwrap paths: candidate for preallocation \
                         (`missing_preallocation`) and error-path hardening.",
                    )
                } else if cyclomatic >= 15 {
                    (
                        "refactor + micro-optimize",
                        "Hot + high cyclomatic complexity: simplify control flow, then \
                         optimize the dominant branch.",
                    )
                } else if centrality > 0.0 {
                    (
                        "optimize critical path",
                        "Hot and central in the call graph: an optimization here \
                         propagates widely.",
                    )
                } else {
                    (
                        "optimize",
                        "Hot leaf. Profile-guided micro-optimization candidate.",
                    )
                };

                resolved.push(json!({
                    "profile_symbol": h.raw,
                    "resolved_name": r.name,
                    "file": r.relative_path,
                    "language": r.language,
                    "line": r.start_line,
                    "intensity": h.intensity,
                    "intensity_unit": h.unit,
                    "function_pagerank": pagerank,
                    "file_pagerank": file_pagerank,
                    "cyclomatic": cyclomatic,
                    "cognitive": r.cognitive.unwrap_or(0),
                    "fan_in": r.fan_in.unwrap_or(0),
                    "fan_out": r.fan_out.unwrap_or(0),
                    "panic_paths": panic_paths,
                    "priority_score": format!("{:.4}", priority),
                    "action": action,
                    "rationale": rationale,
                }));
            }
            None => {
                unresolved.push(json!({
                    "profile_symbol": h.raw,
                    "bare_name": bare,
                    "intensity": h.intensity,
                    "intensity_unit": h.unit,
                }));
            }
        }
    }

    // Sort resolved by priority descending.
    resolved.sort_by(|a, b| {
        let pa: f64 = a["priority_score"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let pb: f64 = b["priority_score"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
    });

    let result = json!({
        "project": project,
        "kind": kind,
        "hot_symbols_parsed": hot.len(),
        "resolved_count": resolved.len(),
        "unresolved_count": unresolved.len(),
        "resolved": resolved,
        "unresolved": unresolved,
        "guidance": "Hot symbols resolved to file:line and ranked by runtime intensity × static \
                     centrality (PageRank) × complexity. Unresolved symbols are external (libc / \
                     kernel / inlined / a language without a symbol backend) or not yet indexed — \
                     run `symbol-extraction` / `call-graph` crons for fuller coverage.",
        "health": health_envelope(graph_present, metrics_present),
    });

    debug!(
        tool = "profile_ingest",
        parsed = hot.len(),
        resolved = resolved.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    json_result(&result)
}

/// Parse the artifact text into ranked hot symbols per `kind`.
fn parse_hot_symbols(kind: &str, content: &str, limit: usize) -> Vec<HotSymbol> {
    match kind {
        "perf" => {
            let mut entries = extract::parse_perf_report(content);
            entries.truncate(limit);
            entries
                .into_iter()
                .map(|e| HotSymbol {
                    raw: e.symbol,
                    intensity: e.self_pct,
                    unit: "self_pct",
                })
                .collect()
        }
        "flamegraph" => {
            let folded = extract::parse_folded_stacks(content);
            let mut v: Vec<(String, u64)> = folded.into_iter().collect();
            v.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
            v.truncate(limit);
            v.into_iter()
                .map(|(sym, count)| HotSymbol {
                    raw: sym,
                    intensity: count as f64,
                    unit: "samples",
                })
                .collect()
        }
        "massif" => {
            let summary = extract::parse_massif(content);
            summary
                .top_frames
                .into_iter()
                .take(limit)
                .map(|f| HotSymbol {
                    raw: f.function,
                    intensity: f.bytes as f64,
                    unit: "bytes",
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Reduce a (possibly mangled / path-qualified) profile symbol to its final
/// identifier segment for `file_symbols.name` matching.
///
/// Handles:
///  - Rust/C++ paths: `myapp::index::build` → `build`, `foo::Bar::method` → `method`.
///  - Generic suffixes: `build<T>` → `build`, `Vec<u8>::push` → `push`.
///  - Itanium-mangled tails: strips a trailing `17h...E` hash that rustc emits
///    when demangling is partial (`_ZN5myapp5buildE` style is rare in `perf`
///    output, which usually demangles; we still guard for the hash suffix).
fn bare_symbol_name(raw: &str) -> String {
    let mut s = raw.trim();
    // Drop a leading `[.] ` / `[k] ` marker if the caller passed a full line.
    if let Some(rest) = s.strip_prefix("[.] ").or_else(|| s.strip_prefix("[k] ")) {
        s = rest.trim();
    }
    // Cut at the first generic-argument `<` so `build<T>` → `build` and
    // `Vec<u8>::push` keeps `::push` for the path split below… but `<` may
    // precede `::`, so split on `::` FIRST, then strip generics on the tail.
    // Take the segment after the last `::`.
    let after_path = s.rsplit("::").next().unwrap_or(s);
    // Strip generic/parameter tails.
    let head = after_path
        .split(['<', '(', ' '])
        .next()
        .unwrap_or(after_path)
        .trim();
    // Strip an Itanium hash suffix like `17h3f2a...` if present.
    let cleaned = strip_rustc_hash(head);
    cleaned.to_string()
}

/// Strip a trailing rustc disambiguator hash (`...17h<hex>`), returning the
/// identifier without it. No-op when the pattern isn't present.
fn strip_rustc_hash(name: &str) -> &str {
    // Pattern: <name>17h<16 hex chars> (sometimes trailing `E`). We look for
    // the literal `17h` followed by hex; conservative so normal names survive.
    if let Some(idx) = name.rfind("17h") {
        let tail = &name[idx + 3..];
        let tail = tail.strip_suffix('E').unwrap_or(tail);
        if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_hexdigit()) {
            return &name[..idx];
        }
    }
    name
}

fn health_envelope(graph_present: bool, metrics_present: bool) -> serde_json::Value {
    json!({
        "callgraph_pagerank_present": graph_present,
        "function_metrics_present": metrics_present,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_name_strips_path_and_generics() {
        assert_eq!(bare_symbol_name("myapp::index::build"), "build");
        assert_eq!(bare_symbol_name("foo::Bar::method"), "method");
        assert_eq!(bare_symbol_name("build<T>"), "build");
        assert_eq!(bare_symbol_name("Vec<u8>::push"), "push");
        assert_eq!(bare_symbol_name("compute_hash"), "compute_hash");
        assert_eq!(bare_symbol_name("[.] memcpy"), "memcpy");
        assert_eq!(bare_symbol_name("do_thing(int)"), "do_thing");
    }

    #[test]
    fn bare_name_strips_rustc_hash() {
        assert_eq!(bare_symbol_name("myapp::build17h3f2a9c1b0d4e5f60"), "build");
        // A normal name containing no valid hash tail is untouched.
        assert_eq!(bare_symbol_name("seventeen"), "seventeen");
    }

    #[test]
    fn parse_hot_symbols_perf() {
        let text = "\
    42.10%  myapp  myapp  [.] compute_hash
    17.55%  myapp  libc   [.] memcpy
";
        let hot = parse_hot_symbols("perf", text, 25);
        assert_eq!(hot.len(), 2);
        assert_eq!(hot[0].raw, "compute_hash");
        assert!((hot[0].intensity - 42.10).abs() < 1e-9);
        assert_eq!(hot[0].unit, "self_pct");
    }

    #[test]
    fn parse_hot_symbols_flamegraph_sorted() {
        let text = "a;b;leaf_lo 10\na;c;leaf_hi 99\n";
        let hot = parse_hot_symbols("flamegraph", text, 25);
        assert_eq!(hot.len(), 2);
        // Sorted by sample count descending.
        assert_eq!(hot[0].raw, "leaf_hi");
        assert!((hot[0].intensity - 99.0).abs() < 1e-9);
    }

    #[test]
    fn parse_hot_symbols_massif() {
        let text = "mem_heap_B=2048\nn1: 2048 0x40: grow (t.rs:1)\n";
        let hot = parse_hot_symbols("massif", text, 25);
        assert!(hot.iter().any(|h| h.raw == "grow" && h.unit == "bytes"));
    }

    #[test]
    fn parse_hot_symbols_unknown_kind_is_empty() {
        assert!(parse_hot_symbols("bogus", "x 1", 25).is_empty());
    }
}
