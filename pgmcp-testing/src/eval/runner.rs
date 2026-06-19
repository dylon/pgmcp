//! Run an evaluation query through a real search tool and reduce the result to
//! a ranked list of [`RankedHit`]s for scoring.
//!
//! The campaign drives the **real** tool handlers via
//! [`McpServer::call_tool_cli`] (the same dispatch + telemetry path as a live
//! MCP call), then parses the JSON envelope. Because every metric in
//! [`pgmcp::quality::retrieval_metrics`] is rank-based, the only thing we need
//! from each result row is its `relative_path` (and, where present, its line
//! span) **in array order** — the array order *is* the ranking. `score` is
//! parsed only for the score-margin diagnostic.

use pgmcp::mcp::server::McpServer;
use pgmcp::quality::retrieval_metrics::{RankedHit, path_dedup};

use crate::eval::rerank::Candidate;

/// The three chunk-granularity search modes under comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchMode {
    /// `semantic_search` — pgvector HNSW cosine over BGE-M3 embeddings.
    Semantic,
    /// `hybrid_search` — RRF fusion of BM25 + semantic (+ optional WFST).
    Hybrid,
    /// `text_search` — Postgres full-text (`ts_rank`).
    Text,
}

impl SearchMode {
    pub fn tool_name(self) -> &'static str {
        match self {
            SearchMode::Semantic => "semantic_search",
            SearchMode::Hybrid => "hybrid_search",
            SearchMode::Text => "text_search",
        }
    }

    pub fn tag(self) -> &'static str {
        match self {
            SearchMode::Semantic => "semantic",
            SearchMode::Hybrid => "hybrid",
            SearchMode::Text => "text",
        }
    }

    pub fn all() -> [SearchMode; 3] {
        [SearchMode::Semantic, SearchMode::Hybrid, SearchMode::Text]
    }

    /// Build the JSON arguments for this mode. We request explicit `fields` for
    /// the field-shaped tools (semantic/text) so the envelope is minimal and
    /// deterministic; `hybrid_search` builds its own envelope and ignores
    /// `fields`, so we parse its richer rows directly.
    fn args(self, query: &str, project: Option<&str>, limit: i32) -> serde_json::Value {
        match self {
            SearchMode::Semantic => serde_json::json!({
                "query": query,
                "project": project,
                "limit": limit,
                "fields": ["relative_path", "start_line", "end_line", "score"],
            }),
            SearchMode::Hybrid => serde_json::json!({
                "query": query,
                "project": project,
                "limit": limit,
            }),
            SearchMode::Text => serde_json::json!({
                "query": query,
                "project": project,
                "limit": limit,
                "fields": ["relative_path", "score"],
            }),
        }
    }
}

/// Extract the single text payload from a `CallToolResult`.
fn text_payload(result: &rmcp::model::CallToolResult) -> Option<String> {
    for c in &result.content {
        if let rmcp::model::RawContent::Text(t) = &c.raw {
            return Some(t.text.clone());
        }
    }
    None
}

/// Parse a search-tool JSON envelope into an ordered list of hits. Handles all
/// three modes: it reads the top-level `results` array and, per row, takes
/// `relative_path`, optional `start_line`/`end_line`, and a score from either
/// `score` (semantic/text) or the string `rrf_score` (hybrid). Rows without a
/// `relative_path` are skipped. Order is preserved (= rank order).
pub fn parse_results(json_text: &str) -> Vec<RankedHit> {
    let v: serde_json::Value = match serde_json::from_str(json_text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let arr = match v.get("results").and_then(|r| r.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut out = Vec::with_capacity(arr.len());
    for row in arr {
        let path = match row.get("relative_path").and_then(|p| p.as_str()) {
            Some(p) => p.to_string(),
            None => continue,
        };
        let start_line = row.get("start_line").and_then(|n| n.as_i64());
        let end_line = row.get("end_line").and_then(|n| n.as_i64());
        let score = row.get("score").and_then(|s| s.as_f64()).or_else(|| {
            row.get("rrf_score")
                .and_then(|s| s.as_str())
                .and_then(|s| s.parse().ok())
        });
        out.push(RankedHit {
            path,
            start_line,
            end_line,
            score,
        });
    }
    out
}

/// Run one query through one mode and return the path-deduped ranked hits.
pub async fn run_mode(
    server: &McpServer,
    mode: SearchMode,
    query: &str,
    project: Option<&str>,
    limit: i32,
) -> Result<Vec<RankedHit>, String> {
    let args = mode.args(query, project, limit);
    let result = server
        .call_tool_cli(mode.tool_name(), args)
        .await
        .map_err(|e| format!("{} call failed: {}", mode.tool_name(), e.message))?;
    if result.is_error == Some(true) {
        let msg = text_payload(&result).unwrap_or_default();
        return Err(format!("{} returned error: {}", mode.tool_name(), msg));
    }
    let text = text_payload(&result)
        .ok_or_else(|| format!("{} returned no text content", mode.tool_name()))?;
    Ok(path_dedup(&parse_results(&text)))
}

/// Fetch the top-`fetch_n` `semantic_search` candidates **with their chunk
/// content** (the rerankers need the passage text, not just the path). Parses
/// the content defensively from `chunk_content` / `snippet` / `content`, and
/// path-dedupes (first occurrence) to match the scoring convention.
pub async fn fetch_semantic_candidates(
    server: &McpServer,
    query: &str,
    project: Option<&str>,
    fetch_n: i32,
) -> Result<Vec<Candidate>, String> {
    let args = serde_json::json!({
        "query": query,
        "project": project,
        "limit": fetch_n,
        "fields": ["relative_path", "start_line", "end_line", "score", "chunk_content"],
    });
    let result = server
        .call_tool_cli("semantic_search", args)
        .await
        .map_err(|e| format!("semantic_search call failed: {}", e.message))?;
    if result.is_error == Some(true) {
        return Err(format!(
            "semantic_search returned error: {}",
            text_payload(&result).unwrap_or_default()
        ));
    }
    let text = text_payload(&result).ok_or("semantic_search returned no text content")?;
    Ok(parse_candidates(&text))
}

/// Fetch the top-`k` candidates for ANY mode **with passage content**, for
/// LLM-judge pooling (Epic 2). `semantic`/`text` request `chunk_content` via
/// `fields`; `hybrid` returns its own `snippet`. (`text_search` currently
/// returns paths only — the judge pooler fills those gaps from the DB.)
/// Path-deduped, order preserved.
pub async fn fetch_candidates(
    server: &McpServer,
    mode: SearchMode,
    query: &str,
    project: Option<&str>,
    k: i32,
) -> Result<Vec<Candidate>, String> {
    let args = match mode {
        SearchMode::Semantic | SearchMode::Text => serde_json::json!({
            "query": query,
            "project": project,
            "limit": k,
            "fields": ["relative_path", "start_line", "end_line", "score", "chunk_content"],
        }),
        SearchMode::Hybrid => serde_json::json!({
            "query": query,
            "project": project,
            "limit": k,
        }),
    };
    let result = server
        .call_tool_cli(mode.tool_name(), args)
        .await
        .map_err(|e| format!("{} call failed: {}", mode.tool_name(), e.message))?;
    if result.is_error == Some(true) {
        return Err(format!(
            "{} returned error: {}",
            mode.tool_name(),
            text_payload(&result).unwrap_or_default()
        ));
    }
    let text = text_payload(&result)
        .ok_or_else(|| format!("{} returned no text content", mode.tool_name()))?;
    Ok(parse_candidates(&text))
}

/// Parse the semantic_search envelope into content-bearing candidates,
/// path-deduped (first occurrence), order preserved.
fn parse_candidates(json_text: &str) -> Vec<Candidate> {
    let v: serde_json::Value = match serde_json::from_str(json_text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let arr = match v.get("results").and_then(|r| r.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(arr.len());
    for row in arr {
        let path = match row.get("relative_path").and_then(|p| p.as_str()) {
            Some(p) => p.to_string(),
            None => continue,
        };
        if !seen.insert(path.clone()) {
            continue; // dedup by path, keep first
        }
        let content = row
            .get("chunk_content")
            .and_then(|c| c.as_str())
            .or_else(|| row.get("snippet").and_then(|c| c.as_str()))
            .or_else(|| row.get("content").and_then(|c| c.as_str()))
            .unwrap_or("")
            .to_string();
        let hit = RankedHit {
            path,
            start_line: row.get("start_line").and_then(|n| n.as_i64()),
            end_line: row.get("end_line").and_then(|n| n.as_i64()),
            score: row.get("score").and_then(|s| s.as_f64()),
        };
        out.push(Candidate { hit, content });
    }
    out
}

/// The graph-augmented retrieval modes, each a distinct MCP tool with its own
/// result shape (all reduced to a file-granularity ranked list for scoring).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GraphMode {
    /// `code_ppr_search` — Personalized PageRank → ranked files.
    Ppr,
    /// `code_path_search` — PathRAG → ranked dependency routes (flattened to files).
    Path,
    /// `code_raptor_search` — RAPTOR module-cluster summaries (flattened to sample files; coarse).
    Raptor,
}

impl GraphMode {
    pub fn tool_name(self) -> &'static str {
        match self {
            GraphMode::Ppr => "code_ppr_search",
            GraphMode::Path => "code_path_search",
            GraphMode::Raptor => "code_raptor_search",
        }
    }

    pub fn tag(self) -> &'static str {
        match self {
            GraphMode::Ppr => "code_ppr",
            GraphMode::Path => "code_path",
            GraphMode::Raptor => "code_raptor",
        }
    }

    pub fn all() -> [GraphMode; 3] {
        [GraphMode::Ppr, GraphMode::Path, GraphMode::Raptor]
    }

    fn args(self, query: &str, project: Option<&str>, k: i32) -> serde_json::Value {
        // ppr/path require a project; raptor's is optional.
        serde_json::json!({ "project": project, "query": query, "k": k })
    }
}

/// Parse a numeric field that the graph tools serialize as a JSON **string**
/// (e.g. `"score": "0.081635"`) or occasionally a number.
fn parse_num(v: &serde_json::Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// Parse `"<start>-<end>"` line ranges (ppr's `lines` field).
fn parse_line_span(v: &serde_json::Value) -> (Option<i64>, Option<i64>) {
    let Some(s) = v.as_str() else {
        return (None, None);
    };
    let mut it = s.split('-');
    let start = it.next().and_then(|x| x.trim().parse().ok());
    let end = it.next().and_then(|x| x.trim().parse().ok());
    (start, end)
}

/// Parse a graph-tool envelope into a file-granularity ranked list, deduped by
/// path (first occurrence). PPR → `results[].file`; PathRAG → `paths[].files`
/// flattened in path order; RAPTOR → `results[].sample_files` flattened in
/// cluster order (coarse — clusters span many files, only samples are returned).
pub fn parse_graph_results(mode: GraphMode, json_text: &str) -> Vec<RankedHit> {
    let v: serde_json::Value = match serde_json::from_str(json_text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<RankedHit> = Vec::new();
    let mut push = |path: String, start: Option<i64>, end: Option<i64>, score: Option<f64>| {
        if seen.insert(path.clone()) {
            out.push(RankedHit {
                path,
                start_line: start,
                end_line: end,
                score,
            });
        }
    };
    match mode {
        GraphMode::Ppr => {
            for row in v
                .get("results")
                .and_then(|r| r.as_array())
                .into_iter()
                .flatten()
            {
                if let Some(file) = row.get("file").and_then(|f| f.as_str()) {
                    let (s, e) = row
                        .get("lines")
                        .map(parse_line_span)
                        .unwrap_or((None, None));
                    push(file.to_string(), s, e, row.get("score").and_then(parse_num));
                }
            }
        }
        GraphMode::Path => {
            for p in v
                .get("paths")
                .and_then(|r| r.as_array())
                .into_iter()
                .flatten()
            {
                let flow = p.get("flow").and_then(parse_num);
                for f in p
                    .get("files")
                    .and_then(|x| x.as_array())
                    .into_iter()
                    .flatten()
                {
                    if let Some(path) = f.as_str() {
                        push(path.to_string(), None, None, flow);
                    }
                }
            }
        }
        GraphMode::Raptor => {
            for c in v
                .get("results")
                .and_then(|r| r.as_array())
                .into_iter()
                .flatten()
            {
                let sim = c.get("similarity").and_then(parse_num);
                for f in c
                    .get("sample_files")
                    .and_then(|x| x.as_array())
                    .into_iter()
                    .flatten()
                {
                    if let Some(path) = f.as_str() {
                        push(path.to_string(), None, None, sim);
                    }
                }
            }
        }
    }
    out
}

/// Run one query through one graph-augmented mode; returns the file-granularity
/// ranked hits (already deduped by `parse_graph_results`).
pub async fn run_graph_mode(
    server: &McpServer,
    mode: GraphMode,
    query: &str,
    project: Option<&str>,
    k: i32,
) -> Result<Vec<RankedHit>, String> {
    let result = server
        .call_tool_cli(mode.tool_name(), mode.args(query, project, k))
        .await
        .map_err(|e| format!("{} call failed: {}", mode.tool_name(), e.message))?;
    if result.is_error == Some(true) {
        return Err(format!(
            "{} returned error: {}",
            mode.tool_name(),
            text_payload(&result).unwrap_or_default()
        ));
    }
    let text = text_payload(&result)
        .ok_or_else(|| format!("{} returned no text content", mode.tool_name()))?;
    Ok(parse_graph_results(mode, &text))
}

/// Fraction of the top-`k` slots occupied by `src/patterns/*` catalog files —
/// the "pattern-catalog crowding" diagnostic. Computed over queries whose gold
/// is *not* itself a pattern file, so a high value means the ~810-entry catalog
/// is displacing genuinely relevant code.
pub fn pattern_crowding_at_k(ranked: &[RankedHit], k: usize) -> f64 {
    let top = ranked.iter().take(k);
    let n = ranked.len().min(k);
    if n == 0 {
        return 0.0;
    }
    let pat = top.filter(|h| h.path.starts_with("src/patterns/")).count();
    pat as f64 / n as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_semantic_envelope() {
        let json = r#"{
            "results": [
                {"relative_path": "src/a.rs", "start_line": 10, "end_line": 60, "score": 0.62},
                {"relative_path": "src/b.rs", "start_line": 1, "end_line": 50, "score": 0.58}
            ],
            "effect_breakdown": {}
        }"#;
        let hits = parse_results(json);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].path, "src/a.rs");
        assert_eq!(hits[0].start_line, Some(10));
        assert!((hits[0].score.unwrap() - 0.62).abs() < 1e-12);
    }

    #[test]
    fn parse_hybrid_envelope_with_string_rrf_and_dupes() {
        // Hybrid emits the same path once per leg with a *string* rrf_score, and
        // text-leg rows have no line span.
        let json = r#"{
            "results": [
                {"relative_path": "src/a.rs", "source": "text", "rrf_score": "0.008197"},
                {"relative_path": "src/a.rs", "source": "semantic", "start_line": 5, "end_line": 55, "rrf_score": "0.008197"},
                {"relative_path": "src/b.rs", "source": "semantic", "start_line": 1, "end_line": 9, "rrf_score": "0.004000"}
            ]
        }"#;
        let hits = parse_results(json);
        assert_eq!(hits.len(), 3, "parse keeps all rows");
        assert!((hits[0].score.unwrap() - 0.008197).abs() < 1e-9);
        // Dedup collapses the duplicated src/a.rs to its first occurrence.
        let deduped = path_dedup(&hits);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].path, "src/a.rs");
        assert_eq!(deduped[1].path, "src/b.rs");
    }

    #[test]
    fn parse_text_envelope_without_spans() {
        let json = r#"{"results": [{"relative_path": "docs/x.md", "score": 0.4}]}"#;
        let hits = parse_results(json);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].start_line, None);
        assert_eq!(hits[0].path, "docs/x.md");
    }

    #[test]
    fn parse_empty_or_malformed_is_empty() {
        assert!(parse_results("not json").is_empty());
        assert!(parse_results(r#"{"no_results": []}"#).is_empty());
        assert!(parse_results(r#"{"results": []}"#).is_empty());
    }

    #[test]
    fn pattern_crowding_counts_patterns_dir() {
        let hits = vec![
            RankedHit::path_only("src/patterns/gof.rs"),
            RankedHit::path_only("src/health/prober.rs"),
            RankedHit::path_only("src/patterns/solid_grasp.rs"),
            RankedHit::path_only("src/db/queries/search.rs"),
        ];
        // top-4: 2 of 4 are pattern files.
        assert!((pattern_crowding_at_k(&hits, 4) - 0.5).abs() < 1e-12);
        // top-2: 1 of 2.
        assert!((pattern_crowding_at_k(&hits, 2) - 0.5).abs() < 1e-12);
    }
}
