//! MCP Server implementation using rmcp.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use arc_swap::ArcSwap;

#[path = "server/error_classify.rs"]
mod error_classify;
use error_classify::*;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::schemars;
use rmcp::service::{NotificationContext, RequestContext};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use rmcp::{tool, tool_handler, tool_router};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tracing::{Instrument, info, info_span, warn};

use crate::config::Config;
use crate::context::SystemContext;
use crate::db::DbClient;
use crate::stats::tracker::StatsTracker;

use super::logging::LogBroadcaster;
use super::tasks::TaskStore;

/// Wrap a tool's delegated future in a `tokio::time::timeout`. Tools
/// that exceed their budget surface a structured `McpError` instead of
/// hanging the harness; clients see a recognizable error rather than
/// dropping the connection. Stage 4b of the pgmcp-utilization plan
/// (`~/.claude/plans/thoroughly-examine-home-dylon-workspace-melodic-cake.md`).
///
/// Default budget is 30 s. `reindex` is the only tool exempt — it can
/// run for minutes when re-indexing a large workspace, and its progress
/// is reported via the MCP task store, not the immediate response.
pub(crate) async fn timeout_wrap<F>(
    name: &str,
    secs: u64,
    fut: F,
) -> Result<CallToolResult, McpError>
where
    F: std::future::Future<Output = Result<CallToolResult, McpError>>,
{
    match tokio::time::timeout(std::time::Duration::from_secs(secs), fut).await {
        Ok(r) => r,
        Err(_) => Err(McpError::internal_error(
            format!("{} timed out after {}s", name, secs),
            None,
        )),
    }
}

/// Identifying metadata about the MCP peer that issued the current call,
/// derived from the rmcp `RequestContext`. `client_name` is normalized to
/// lowercase so per-(tool, client) breakdowns are stable across capitalization
/// variants in `clientInfo.name`.
///
/// `client_version` and `protocol_version` are captured for Tier 3's DB-row
/// telemetry but unused by the Tier 1 in-memory counters. The
/// `#[allow(dead_code)]` is removed once those fields land in the
/// `mcp_tool_calls` row builder.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct CallerInfo {
    pub client_name: String,
    pub client_version: String,
    pub protocol_version: String,
}

impl CallerInfo {
    pub fn unknown() -> Self {
        Self {
            client_name: "unknown".to_string(),
            client_version: "unknown".to_string(),
            protocol_version: "unknown".to_string(),
        }
    }
}

/// Extract caller identification from the rmcp `RequestContext`. Reads
/// `ctx.peer.peer_info()` (which carries `client_info.{name, version}` and
/// `protocol_version` from the MCP `initialize` handshake) and returns
/// a `CallerInfo`. Falls back to `"unknown"` triple when the peer has not
/// completed `initialize` yet (rare; only on the very first tool call).
pub(crate) fn extract_caller(ctx: &RequestContext<RoleServer>) -> CallerInfo {
    let Some(info) = ctx.peer.peer_info() else {
        return CallerInfo::unknown();
    };
    CallerInfo {
        client_name: info.client_info.name.to_lowercase(),
        client_version: info.client_info.version.clone(),
        // `info.protocol_version` is `rmcp::model::ProtocolVersion`,
        // a newtype around a `&'static str` / `String`. Use `Display`
        // for a stable wire-format string ("2024-11-05") instead of
        // the `Debug` repr — telemetry rows / dashboards would
        // otherwise silently change shape if rmcp updates its Debug
        // impl.
        protocol_version: info.protocol_version.to_string(),
    }
}

/// Time, identify, and record a tool invocation, then forward through
/// `timeout_wrap` so the timeout behavior is preserved verbatim. Replaces
/// the older `self.stats().record_tool_call(name)` + `timeout_wrap(...)`
/// idiom that lived at the top of every `#[tool]` body — captures the
/// same per-tool counter PLUS duration, error outcome, and caller identity
/// in one place. Additionally enqueues a durable `TelemetryRow` for the
/// async writer task (Tier 3); the enqueue is non-blocking and drops on
/// channel overflow.
///
/// Also emits `info!` events at entry (`"invoked"`) and exit (`"completed"`
/// on success, `warn!("failed", ...)` on error) under the
/// `pgmcp::mcp::tool` tracing target so every MCP tool call lands in the
/// daemon log file. `params_summary` is a compact one-line summary of the
/// parameters (typically `summarize_debug(&params)`); pass `""` for nullary
/// tools.
pub(crate) async fn instrumented_tool_wrap<F>(
    stats: &StatsTracker,
    name: &str,
    secs: u64,
    ctx: &RequestContext<RoleServer>,
    params_summary: &str,
    fut: F,
) -> Result<CallToolResult, McpError>
where
    F: std::future::Future<Output = Result<CallToolResult, McpError>>,
{
    let caller = extract_caller(ctx);
    let request_id = Some(format!("{:?}", ctx.id));
    instrumented_tool_run(
        stats,
        name,
        Some(secs),
        caller,
        params_summary,
        request_id,
        fut,
    )
    .await
}

/// Inner instrumentation that does not require an rmcp `RequestContext`.
/// Used by both the MCP transport path (via `instrumented_tool_wrap`) and
/// the CLI dispatch path (`call_tool_cli`). Pass `timeout_secs = None` for
/// long-running tools that should not be bounded by `timeout_wrap`
/// (e.g. `reindex`).
pub(crate) async fn instrumented_tool_run<F>(
    stats: &StatsTracker,
    name: &str,
    timeout_secs: Option<u64>,
    caller: CallerInfo,
    params_summary: &str,
    request_id: Option<String>,
    fut: F,
) -> Result<CallToolResult, McpError>
where
    F: std::future::Future<Output = Result<CallToolResult, McpError>>,
{
    let span = info_span!(
        target: "pgmcp::mcp::tool",
        "mcp_tool",
        tool = name,
        client = %caller.client_name,
    );
    info!(
        target: "pgmcp::mcp::tool",
        tool = name,
        client = %caller.client_name,
        params = %params_summary,
        "invoked",
    );
    let start = std::time::Instant::now();
    let result = match timeout_secs {
        Some(s) => timeout_wrap(name, s, fut).instrument(span).await,
        None => fut.instrument(span).await,
    };
    let elapsed = start.elapsed();
    let elapsed_ms = elapsed.as_millis().min(u64::MAX as u128) as u64;
    let duration_ns = elapsed.as_nanos().min(u64::MAX as u128) as u64;
    let ok = result.is_ok();
    stats.record_tool_call(name, &caller.client_name, duration_ns, ok);
    match &result {
        Ok(_) => info!(
            target: "pgmcp::mcp::tool",
            tool = name,
            client = %caller.client_name,
            duration_ms = elapsed_ms,
            "completed",
        ),
        Err(e) => {
            stats.mcp_errors.fetch_add(1, Ordering::Relaxed);
            warn!(
                target: "pgmcp::mcp::tool",
                tool = name,
                client = %caller.client_name,
                duration_ms = elapsed_ms,
                error = %e,
                "failed",
            );
        }
    }
    // `classify_result` needs a concrete timeout budget; for unbounded
    // calls (None) we use u64::MAX so the elapsed-vs-budget check never
    // misclassifies a long-running success/error as a timeout.
    let timeout_for_classify = timeout_secs.unwrap_or(u64::MAX);
    let (outcome, error_class) = classify_result(&result, timeout_for_classify, elapsed);
    // Hash the params summary so telemetry rows carry a stable join key
    // for downstream analyses (deduplicating identical-shape invocations
    // across clients, agents, and time windows). Empty params hash to
    // `None` so analyses can distinguish nullary tools from parametrized
    // tools with truly identical params.
    let params_sha256 = if params_summary.is_empty() {
        None
    } else {
        Some(format!("{:x}", Sha256::digest(params_summary.as_bytes())))
    };
    let row = crate::stats::telemetry_writer::TelemetryRow {
        tool: name.to_string(),
        client_name: caller.client_name.clone(),
        client_version: Some(caller.client_version.clone()),
        protocol_version: Some(caller.protocol_version.clone()),
        mcp_session_id: None,
        project: None,
        cwd: None,
        duration_ms: (duration_ns / 1_000_000).min(i32::MAX as u64) as i32,
        outcome,
        error_class,
        request_id,
        params_sha256,
    };
    crate::stats::telemetry_writer::try_enqueue(stats, row);
    result
}

/// Compact one-line summary of a tool's typed parameters for logging.
/// Uses `Debug` (every `*Params` struct in this file derives it). Truncates
/// to 200 bytes on a valid UTF-8 char boundary with a `…(+NB)` suffix
/// indicating how many bytes were elided.
pub(crate) fn summarize_debug<P: std::fmt::Debug + ?Sized>(p: &P) -> String {
    truncate_for_log(&format!("{:?}", p))
}

/// Compact one-line summary of a raw JSON params value (used by the CLI
/// dispatch path which receives `serde_json::Value` rather than a typed
/// struct). Uses `Value::to_string` for readable JSON shape.
pub(crate) fn summarize_json(v: &serde_json::Value) -> String {
    truncate_for_log(&v.to_string())
}

fn truncate_for_log(s: &str) -> String {
    if s.len() <= 200 {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(200);
        format!("{}…(+{}B)", &s[..end], s.len() - end)
    }
}

/// Classify a tool result into the `outcome` + `error_class` columns of
/// `mcp_tool_calls`. The `timeout` outcome is detected when the duration
/// is at-or-above the budget AND the error message mentions "timed out".
fn classify_result(
    result: &Result<CallToolResult, McpError>,
    timeout_secs: u64,
    elapsed: std::time::Duration,
) -> (&'static str, Option<String>) {
    match result {
        Ok(_) => ("ok", None),
        Err(e) => {
            let msg = e.to_string();
            let is_timeout = elapsed.as_secs() >= timeout_secs && msg.contains("timed out");
            if is_timeout {
                ("timeout", Some("timeout".to_string()))
            } else {
                ("error", Some(classify_error_kind(&msg)))
            }
        }
    }
}

/// Truncate a string to at most `max_len` bytes on a valid char boundary.
pub(crate) fn truncate(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        &s[..s.floor_char_boundary(max_len)]
    }
}

// ============================================================================
// Union-Find for duplicate clustering
// ============================================================================

pub(crate) struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<usize>,
}

impl UnionFind {
    pub(crate) fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    pub(crate) fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            self.parent[x] = self.find(self.parent[x]);
        }
        self.parent[x]
    }

    pub(crate) fn union(&mut self, x: usize, y: usize) {
        let rx = self.find(x);
        let ry = self.find(y);
        if rx == ry {
            return;
        }
        if self.rank[rx] < self.rank[ry] {
            self.parent[rx] = ry;
        } else if self.rank[rx] > self.rank[ry] {
            self.parent[ry] = rx;
        } else {
            self.parent[ry] = rx;
            self.rank[rx] += 1;
        }
    }
}

/// Cluster duplicate file pairs using union-find.
/// Returns clusters that span at least `min_projects` distinct projects.
pub(crate) fn cluster_file_pairs(
    pairs: &[crate::db::queries::DuplicateFilePair],
    min_projects: usize,
) -> Vec<serde_json::Value> {
    use std::collections::{HashMap, HashSet};

    if pairs.is_empty() {
        return Vec::new();
    }

    // Assign each unique file_id an index
    let mut file_ids: Vec<i64> = Vec::new();
    let mut id_to_idx: HashMap<i64, usize> = HashMap::new();

    for pair in pairs {
        if let std::collections::hash_map::Entry::Vacant(e) = id_to_idx.entry(pair.file_id_a) {
            e.insert(file_ids.len());
            file_ids.push(pair.file_id_a);
        }
        if let std::collections::hash_map::Entry::Vacant(e) = id_to_idx.entry(pair.file_id_b) {
            e.insert(file_ids.len());
            file_ids.push(pair.file_id_b);
        }
    }

    // Build file metadata map
    struct FileMeta {
        path: String,
        project_name: String,
        project_id: i32,
        language: String,
        line_count: Option<i64>,
    }

    let mut meta: HashMap<i64, FileMeta> = HashMap::new();
    for pair in pairs {
        meta.entry(pair.file_id_a).or_insert_with(|| FileMeta {
            path: pair.path_a.clone(),
            project_name: pair.project_name_a.clone(),
            project_id: pair.project_id_a,
            language: pair.language.clone(),
            line_count: None,
        });
        meta.entry(pair.file_id_b).or_insert_with(|| FileMeta {
            path: pair.path_b.clone(),
            project_name: pair.project_name_b.clone(),
            project_id: pair.project_id_b,
            language: pair.language.clone(),
            line_count: None,
        });
    }

    // Union-find clustering
    let mut uf = UnionFind::new(file_ids.len());
    let mut pair_sims: HashMap<(usize, usize), f64> = HashMap::new();
    for pair in pairs {
        let ia = id_to_idx[&pair.file_id_a];
        let ib = id_to_idx[&pair.file_id_b];
        uf.union(ia, ib);
        pair_sims.insert((ia.min(ib), ia.max(ib)), pair.avg_similarity);
    }

    // Collect clusters
    let mut clusters: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..file_ids.len() {
        let root = uf.find(i);
        clusters.entry(root).or_default().push(i);
    }

    // Filter to clusters spanning min_projects and format output
    let mut result: Vec<serde_json::Value> = Vec::new();
    for members in clusters.values() {
        let mut projects: HashSet<i32> = HashSet::new();
        let mut project_names: HashSet<String> = HashSet::new();
        let mut files = Vec::new();
        let mut language = String::new();
        let mut sim_sum = 0.0f64;
        let mut sim_count = 0u64;

        for &idx in members {
            let fid = file_ids[idx];
            if let Some(m) = meta.get(&fid) {
                projects.insert(m.project_id);
                project_names.insert(m.project_name.clone());
                language = m.language.clone();

                // Extract relative_path from absolute path (last path components after project root)
                let rel_path = m
                    .path
                    .rsplit('/')
                    .take(4)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("/");
                files.push(serde_json::json!({
                    "file_id": fid,
                    "path": m.path,
                    "relative_path": rel_path,
                    "project": m.project_name,
                    "line_count": m.line_count,
                }));
            }
        }

        // Calculate average similarity across all pairs in this cluster
        for i in 0..members.len() {
            for j in (i + 1)..members.len() {
                let key = (members[i].min(members[j]), members[i].max(members[j]));
                if let Some(&sim) = pair_sims.get(&key) {
                    sim_sum += sim;
                    sim_count += 1;
                }
            }
        }

        if projects.len() < min_projects {
            continue;
        }

        let avg_sim = if sim_count > 0 {
            sim_sum / sim_count as f64
        } else {
            0.0
        };

        result.push(serde_json::json!({
            "cluster_size": members.len(),
            "projects": project_names.into_iter().collect::<Vec<_>>(),
            "project_count": projects.len(),
            "language": language,
            "avg_similarity": format!("{:.4}", avg_sim),
            "files": files,
            "representative_file": files.first(),
        }));
    }

    // Sort by project_count * avg_similarity descending
    result.sort_by(|a, b| {
        let score_a = a["project_count"].as_u64().unwrap_or(0) as f64
            * a["avg_similarity"]
                .as_str()
                .unwrap_or("0")
                .parse::<f64>()
                .unwrap_or(0.0);
        let score_b = b["project_count"].as_u64().unwrap_or(0) as f64
            * b["avg_similarity"]
                .as_str()
                .unwrap_or("0")
                .parse::<f64>()
                .unwrap_or(0.0);
        score_b
            .partial_cmp(&score_a)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    result
}

/// Infer a suggested crate name from common path segments across files.
pub(crate) fn infer_crate_name(paths: &[&str]) -> String {
    if paths.is_empty() {
        return "shared-lib".to_string();
    }

    // Find common path segments (ignoring project root differences)
    // Take the last meaningful segment that appears in most paths
    let mut segment_counts: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    for path in paths {
        let segments: std::collections::HashSet<&str> = path
            .split('/')
            .filter(|s| !s.is_empty() && *s != "src" && *s != "mod.rs" && !s.contains('.'))
            .collect();
        for seg in segments {
            *segment_counts.entry(seg).or_insert(0) += 1;
        }
    }

    // Find the segment that appears in the most paths (excluding very generic ones)
    let generic = ["lib", "main", "index", "utils", "helpers", "common"];
    segment_counts
        .into_iter()
        .filter(|(seg, count)| *count > 1 && !generic.contains(seg))
        .max_by_key(|(_, count)| *count)
        .map(|(seg, _)| seg.replace('_', "-"))
        .unwrap_or_else(|| "shared-lib".to_string())
}

/// MCP Server state.
#[derive(Clone)]
pub struct McpServer {
    /// Bundled dependencies (db, embed, stats, config, log, tasks).
    /// Tool methods access these via accessor methods (`self.db()`,
    /// `self.stats()`, etc.) which delegate to the context.
    ctx: SystemContext,
    tool_router: ToolRouter<McpServer>,
}

// Accessor methods that delegate to the SystemContext. Lets the existing
// tool method bodies keep using `self.<accessor>()` identically; the
// constructor surface and the stored state collapse to a single field.
impl McpServer {
    fn db(&self) -> &Arc<dyn DbClient> {
        self.ctx.db()
    }
    #[allow(dead_code)] // Kept for parity; tool bodies have all migrated to ctx.embed() directly.
    fn embed_source(&self) -> &crate::embed::EmbedSource {
        self.ctx.embed()
    }
    fn stats(&self) -> &Arc<StatsTracker> {
        self.ctx.stats()
    }
    #[allow(dead_code)]
    fn config(&self) -> &Arc<ArcSwap<Config>> {
        self.ctx.config()
    }
    fn log_broadcaster(&self) -> &Arc<LogBroadcaster> {
        self.ctx.log_broadcaster()
    }
    fn task_store(&self) -> &Arc<TaskStore> {
        self.ctx.task_store()
    }

    /// Expose the context for `src/mcp/tools/*.rs` free functions to
    /// receive as `&SystemContext` when migrating tool bodies out of this
    /// file.
    pub(crate) fn ctx(&self) -> &SystemContext {
        &self.ctx
    }
}

// === Tool parameter types ===

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SemanticSearchParams {
    #[schemars(description = "Search query text")]
    pub query: String,
    #[schemars(description = "Maximum number of results (default: 10)")]
    pub limit: Option<i32>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(description = "Filter by project name")]
    pub project: Option<String>,
    #[schemars(
        description = "If true, collapse cross-worktree duplicates (same file appearing \
                       in multiple worktrees / sibling clones of the same upstream repo) \
                       to a single canonical hit per (repo, relative_path). Default false: \
                       all hits are returned, including the same code on different branches."
    )]
    pub dedupe_worktrees: Option<bool>,
    // Shadow-ASR filter params (Pattern D): restrict to chunks whose
    // enclosing symbol carries the given return_type_tags / effects /
    // scope_kind. Optional; omitting them preserves legacy behavior.
    #[schemars(
        description = "Restrict hits to chunks whose enclosing symbol's return_type_tags contains \
                       ALL of these tags (subset semantics). Optional."
    )]
    pub return_type_tags: Option<Vec<String>>,
    #[schemars(
        description = "Restrict hits to chunks whose enclosing symbol carries at least one of \
                       these effects. Optional."
    )]
    pub effects: Option<Vec<String>>,
    #[schemars(
        description = "Restrict hits to chunks whose enclosing symbol kind matches (e.g. \
                       \"function\", \"trait\", \"class\"). Optional."
    )]
    pub scope_kind: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TextSearchParams {
    #[schemars(description = "Full-text search query")]
    pub query: String,
    #[schemars(description = "Maximum number of results (default: 10)")]
    pub limit: Option<i32>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(
        description = "If true, collapse cross-worktree duplicates (see semantic_search). \
                       Default false."
    )]
    pub dedupe_worktrees: Option<bool>,
    #[schemars(
        description = "Shadow-ASR filter: restrict hits to chunks whose enclosing symbol's \
                       return_type_tags contains ALL of these tags. Optional."
    )]
    pub return_type_tags: Option<Vec<String>>,
    #[schemars(
        description = "Shadow-ASR filter: restrict hits to chunks whose enclosing symbol carries \
                       at least one of these effects. Optional."
    )]
    pub effects: Option<Vec<String>>,
    #[schemars(
        description = "Shadow-ASR filter: restrict hits to chunks whose enclosing symbol kind \
                       matches. Optional."
    )]
    pub scope_kind: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GrepParams {
    #[schemars(
        description = "Regex pattern to search for (or, when fuzzy=true, a TokenGrep query)"
    )]
    pub pattern: String,
    #[schemars(
        description = "If true, match `pattern` APPROXIMATELY (liblevenshtein TokenGrep) across \
                       indexed file_chunks instead of exact regex — finds typo'd / near-miss \
                       identifiers. Strongly recommend setting `project` to bound the scan. \
                       Default false."
    )]
    pub fuzzy: Option<bool>,
    #[schemars(description = "Max edit distance per token when fuzzy=true (default 2).")]
    pub fuzzy_max_distance: Option<u32>,
    #[schemars(description = "Glob pattern to filter files (e.g. '*.rs')")]
    pub glob: Option<String>,
    #[schemars(description = "Maximum number of results (default: 10)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "If true, collapse cross-worktree duplicates (see semantic_search). \
                       Default false."
    )]
    pub dedupe_worktrees: Option<bool>,
    #[schemars(description = "Filter matches to a specific project (by name)")]
    pub project: Option<String>,
    #[schemars(description = "Filter matches to a specific language string \
                       (e.g. \"rust\", \"pdf\", \"latex\")")]
    pub language: Option<String>,
    #[schemars(
        description = "Lines of context to show BEFORE each match (default: 0). Returns at most \
                       this many extra lines from the matching chunk to anchor the hit; \
                       cross-chunk context-line stitching is not performed."
    )]
    pub before_context: Option<i32>,
    #[schemars(
        description = "Lines of context to show AFTER each match (default: 0). See \
                       `before_context` for caveats."
    )]
    pub after_context: Option<i32>,
    #[schemars(description = "If true, ignore case (`~*` regex op). Default false.")]
    pub case_insensitive: Option<bool>,
    #[schemars(
        description = "Shadow-ASR filter: restrict hits to chunks whose enclosing symbol's \
                       return_type_tags contains ALL of these tags. Optional."
    )]
    pub return_type_tags: Option<Vec<String>>,
    #[schemars(
        description = "Shadow-ASR filter: restrict hits to chunks whose enclosing symbol carries \
                       at least one of these effects. Optional."
    )]
    pub effects: Option<Vec<String>>,
    #[schemars(
        description = "Shadow-ASR filter: restrict hits to chunks whose enclosing symbol kind \
                       matches. Optional."
    )]
    pub scope_kind: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchCommitsParams {
    #[schemars(
        description = "Search query text (matched by semantic similarity against commit messages and diffs)"
    )]
    pub query: String,
    #[schemars(description = "Maximum number of results (default: 10)")]
    pub limit: Option<i32>,
    #[schemars(description = "Filter by project name")]
    pub project: Option<String>,
    #[schemars(
        description = "Shadow-ASR filter: restrict to commits that touched files containing \
                       at least one symbol carrying any of these effects (e.g. ['unsafe', \
                       'crypto'] surfaces commits that introduced unsafe-or-crypto code). \
                       Optional."
    )]
    pub touched_effects: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SoftwarePatternSearchParams {
    #[schemars(description = "Design/problem query to match against the software pattern index")]
    pub query: String,
    #[schemars(description = "Maximum number of pattern matches to return (default: 10)")]
    pub limit: Option<i32>,
    #[schemars(description = "Filter to pattern or anti_pattern")]
    pub kind: Option<String>,
    #[schemars(
        description = "Programming paradigms to target, e.g. object_oriented_programming, functional_programming, logic_programming, event_driven_programming, concurrent_programming, parallel_programming, aspect_oriented_programming"
    )]
    pub paradigms: Option<Vec<String>>,
    #[schemars(
        description = "Filter by pattern category, e.g. creational, behavioral, resilience"
    )]
    pub category: Option<String>,
    #[schemars(description = "Filter by source family, e.g. wikipedia, oodesign, aws, aspectj")]
    pub source_family: Option<String>,
    #[schemars(
        description = "Filter by source type, e.g. curated_card, article, manual, repository"
    )]
    pub source_type: Option<String>,
    #[schemars(description = "Include source metadata and bounded excerpts (default: true)")]
    pub include_sources: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecommendDesignPatternsParams {
    #[schemars(description = "Feature or refactor task to design")]
    pub task: String,
    #[schemars(
        description = "Target programming paradigms. If omitted, inferred from language/project."
    )]
    pub paradigms: Option<Vec<String>>,
    #[schemars(description = "Implementation language, used for paradigm inference")]
    pub language: Option<String>,
    #[schemars(description = "Project name, used for dominant-language inference")]
    pub project: Option<String>,
    #[schemars(description = "Design constraints, risks, or preferences")]
    pub constraints: Option<Vec<String>>,
    #[schemars(description = "Maximum number of recommended patterns (default: 8)")]
    pub limit: Option<i32>,
    #[schemars(description = "Include anti-patterns to avoid (default: true)")]
    pub include_antipatterns: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReviewDesignPatternsParams {
    #[schemars(description = "Proposed design to review")]
    pub design: String,
    #[schemars(
        description = "Target programming paradigms. If omitted, inferred from language/project."
    )]
    pub paradigms: Option<Vec<String>>,
    #[schemars(description = "Implementation language, used for paradigm inference")]
    pub language: Option<String>,
    #[schemars(description = "Project name, used for dominant-language inference")]
    pub project: Option<String>,
    #[schemars(description = "Maximum number of findings/matches (default: 8)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetSoftwarePatternParams {
    #[schemars(description = "Pattern slug or numeric id")]
    pub slug_or_id: String,
    #[schemars(description = "Include source metadata (default: true)")]
    pub include_sources: Option<bool>,
    #[schemars(description = "Include bounded source excerpts (default: false)")]
    pub include_excerpts: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListSoftwarePatternsParams {
    #[schemars(description = "Filter to pattern or anti_pattern")]
    pub kind: Option<String>,
    #[schemars(description = "Filter by paradigm slug or name")]
    pub paradigm: Option<String>,
    #[schemars(description = "Filter by category")]
    pub category: Option<String>,
    #[schemars(description = "Filter by source family")]
    pub source_family: Option<String>,
    #[schemars(description = "Maximum number of rows (default: 50)")]
    pub limit: Option<i32>,
    #[schemars(description = "Offset for pagination (default: 0)")]
    pub offset: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RefreshPatternCatalogParams {
    #[schemars(
        description = "Refresh mode: seed_only, source_family, or all. seed_only embeds bundled cards; source_family/all fetch opted-in source URLs."
    )]
    pub mode: Option<String>,
    #[schemars(description = "Source family to import when mode=source_family, e.g. oodesign")]
    pub source_family: Option<String>,
    #[schemars(description = "If true, report what would be imported without changing the DB")]
    pub dry_run: Option<bool>,
    #[schemars(description = "Maximum sources to import for this run")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpsertPatternSourceParams {
    #[schemars(description = "Existing pattern slug to attach this source to")]
    pub pattern_slug: String,
    #[schemars(description = "Source family label, e.g. local, team_wiki, oodesign")]
    pub source_family: String,
    #[schemars(description = "Source type, e.g. article, manual, snippet, repository")]
    pub source_type: String,
    #[schemars(description = "Source title")]
    pub title: String,
    #[schemars(description = "Optional source URL")]
    pub url: Option<String>,
    #[schemars(description = "Optional license/provenance label")]
    pub license_label: Option<String>,
    #[schemars(description = "Full text content to chunk and embed")]
    pub content: String,
    #[schemars(description = "Rebuild chunks/embeddings for this source (default: true)")]
    pub reembed: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CompareFilesParams {
    #[schemars(description = "First file reference (project:relative_path or absolute path)")]
    pub file_a: String,
    #[schemars(description = "Second file reference (project:relative_path or absolute path)")]
    pub file_b: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindSimilarModulesParams {
    #[schemars(description = "Project name containing the module")]
    pub project: String,
    #[schemars(
        description = "Module path pattern (glob/substring match, e.g. 'work_pool' or 'src/cron')"
    )]
    pub module_path: String,
    #[schemars(description = "Minimum similarity threshold (default: 0.80)")]
    pub min_similarity: Option<f64>,
    #[schemars(description = "Maximum number of results (default: 20)")]
    pub limit: Option<i32>,
    #[schemars(description = "Filter results to a specific target project")]
    pub target_project: Option<String>,
    #[schemars(
        description = "If true, also return matches in worktrees / sibling clones \
                       of the seed file's repo (same git_common_dir or \
                       git_root_commits). Default false — same-repo matches are \
                       excluded so cross-repo refactor candidates aren't drowned \
                       out by the same code on different branches."
    )]
    pub include_same_repo: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindDuplicatesParams {
    #[schemars(description = "Minimum similarity threshold (default: 0.90)")]
    pub min_similarity: Option<f64>,
    #[schemars(description = "Minimum number of projects a cluster must span (default: 2)")]
    pub min_projects: Option<usize>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(description = "Maximum number of clusters to return (default: 20)")]
    pub limit: Option<i32>,
    #[schemars(description = "If true, include duplicates whose two projects are \
                       worktrees / sibling clones of the same upstream repo \
                       (same git_common_dir or git_root_commits). Default false. \
                       Most operators want false: same-code-different-branch is \
                       not a refactor candidate.")]
    pub include_same_repo: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RefactoringReportParams {
    #[schemars(description = "Minimum similarity threshold (default: 0.85)")]
    pub min_similarity: Option<f64>,
    #[schemars(description = "Minimum number of projects a cluster must span (default: 2)")]
    pub min_projects: Option<usize>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(description = "Maximum number of candidates to return (default: 20)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "If true, include refactor candidates whose two projects \
                       are worktrees / sibling clones of the same upstream repo. \
                       Default false."
    )]
    pub include_same_repo: Option<bool>,
}

/// Tier 2 — `chunk_clusters` (chunk-level cross-project DRY).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ChunkClustersParams {
    #[schemars(
        description = "Minimum chunk-pair similarity. Threshold for clustering decisions (default: 0.88)."
    )]
    pub min_similarity: Option<f64>,
    #[schemars(description = "Minimum chunks per cluster (default: 3)")]
    pub min_cluster_size: Option<usize>,
    #[schemars(
        description = "Minimum distinct projects a cluster must span (default: 2). Set 1 for intra-project."
    )]
    pub min_projects: Option<usize>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(
        description = "Filter pairs to those touching this project. Use to focus the audit on a single project's DRY violations."
    )]
    pub project: Option<String>,
    #[schemars(description = "Maximum clusters to return (default: 20)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "Worktree filter: \"main\" (default) restricts both endpoints to canonical \
                       main projects (e.g. f1r3node/, not f1r3node-reified-rspaces/). \"all\" \
                       allows feature-branch worktrees."
    )]
    pub worktree_filter: Option<String>,
    #[schemars(
        description = "If true, include pairs whose two projects are worktrees / sibling clones \
                       of the same upstream repo. Default false (cross-repo refactor candidates only)."
    )]
    pub include_same_repo: Option<bool>,
}

// ============================================================================
// Tier 5 — Audit & trend params
// ============================================================================

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DependencyHealthParams {
    #[schemars(description = "Filter to a single project (optional)")]
    pub project: Option<String>,
    #[schemars(
        description = "Worktree filter: \"main\" (default) restricts to canonical main projects."
    )]
    pub worktree_filter: Option<String>,
    #[schemars(description = "Maximum dependency entries to return (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PatternSearchParams {
    #[schemars(description = "Code snippet to find similar implementations for")]
    pub snippet: String,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(description = "Minimum cosine similarity (default: 0.78)")]
    pub min_similarity: Option<f64>,
    #[schemars(description = "Maximum matches to return (default: 15)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "Exclude this project from results (typically the caller's own project)"
    )]
    pub exclude_project: Option<String>,
    #[schemars(description = "Worktree filter: \"main\" (default) or \"all\"")]
    pub worktree_filter: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MergeConflictRiskParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Files in the in-flight branch (relative paths, required)")]
    pub branch_files: Vec<String>,
    #[schemars(description = "Lookback window in days (default: 14)")]
    pub window_days: Option<i32>,
    #[schemars(
        description = "Exclude this author email from the risk count (typically the PR author)"
    )]
    pub exclude_email: Option<String>,
    #[schemars(description = "Maximum files to return (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NamingConsistencyParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Filter by programming language (e.g. \"rust\", \"python\"). Omit to scan every language with a registered backend."
    )]
    pub language: Option<String>,
    #[schemars(
        description = "Minimum dominance for the per-(directory, kind) convention (default: 0.7). Below this threshold the directory is considered too mixed to flag divergences."
    )]
    pub min_dominance: Option<f64>,
    #[schemars(description = "Maximum divergences to return (default: 50)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "Whether to embed `recommended_fix` per divergence (default: true). Set false to reproduce the diagnostic-only shape."
    )]
    pub include_fixes: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ModuleGrowthParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Single-file path (optional). When omitted, project-scope.")]
    pub file: Option<String>,
    #[schemars(description = "Time bucket: \"week\", \"month\" (default), or \"quarter\".")]
    pub bucket: Option<String>,
    #[schemars(description = "How many buckets back to look at (default: 12)")]
    pub lookback_buckets: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AdoptionLagParams {
    #[schemars(description = "Reference file (the modern implementation)")]
    pub new_file: String,
    #[schemars(description = "Project filter (optional)")]
    pub project: Option<String>,
    #[schemars(description = "Worktree filter: \"main\" (default) or \"all\"")]
    pub worktree_filter: Option<String>,
    #[schemars(description = "Minimum similarity for legacy candidates (default: 0.70)")]
    pub min_similarity: Option<f64>,
    #[schemars(
        description = "Minimum age in days for a file to be considered legacy (default: 180)"
    )]
    pub legacy_min_age_days: Option<i32>,
    #[schemars(description = "Maximum legacy usages to return (default: 30)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TechDebtBurnDownParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Time horizon: \"week\", \"month\" (default), or \"quarter\".")]
    pub time_horizon: Option<String>,
    #[schemars(description = "Number of engineers available (default: 1)")]
    pub engineer_count: Option<i32>,
    #[schemars(description = "Maximum items to consider (default: 50)")]
    pub limit: Option<i32>,
}

/// Tier 4 — `pr_scope_recommender` (min/recommended/max PR scope from a starter file).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PrScopeRecommenderParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Starter file (relative path, required)")]
    pub file: String,
    #[schemars(
        description = "Minimum co-change Jaccard for the recommended scope (default: 0.4)."
    )]
    pub co_change_min: Option<f64>,
    #[schemars(description = "Reverse-BFS depth for the maximum scope (default: 2).")]
    pub impact_depth: Option<i32>,
    #[schemars(
        description = "If true (default), include topic-neighbor files (chunks sharing the seed's \
                       dominant topic) in the maximum scope."
    )]
    pub include_topic_neighbors: Option<bool>,
}

/// Tier 4 — `hot_path_audit` (central + churning + bug-prone intersection).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HotPathAuditParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Percentile threshold (default: 0.9 = top 10%). A file qualifies only if \
                       it sits in the top P% of pagerank, churn, AND fix_commit_ratio."
    )]
    pub percentile_threshold: Option<f64>,
    #[schemars(description = "Maximum hot paths to return (default: 20)")]
    pub limit: Option<i32>,
}

/// Tier 4 — `bus_factor_map` (knowledge-concentration risk per file).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BusFactorMapParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Filter to files in the top (1 - min_pagerank_percentile) of pagerank \
                       (default: 0.5 — top half). Less central files are filtered out."
    )]
    pub min_pagerank_percentile: Option<f64>,
    #[schemars(description = "Maximum files to return (default: 30)")]
    pub limit: Option<i32>,
}

/// Tier 4 — `reviewer_recommender` (rank reviewers by recent file ownership).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReviewerRecommenderParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Changed files (relative paths, required)")]
    pub files: Vec<String>,
    #[schemars(description = "Authors to exclude (e.g. the PR author's email). Optional.")]
    pub exclude_authors: Option<Vec<String>>,
    #[schemars(
        description = "Recency window in days for blame data (default: 365). Older blame is \
                       ignored — long-stale ownership isn't reviewer authority."
    )]
    pub recency_window_days: Option<i32>,
}

/// Tier 3 — `recommend_layering` (infer layered architecture, list violation edges).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecommendLayeringParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Number of layers to bucket the project into (default: 4)")]
    pub num_layers: Option<usize>,
    #[schemars(
        description = "Minimum severity to report: \"low\", \"medium\", \"high\", \"critical\" \
                       (default: \"medium\"). Severity = number of layers an edge crosses."
    )]
    pub severity_threshold: Option<String>,
    #[schemars(description = "Maximum violations to return (default: 50)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "Optional layer-name override (top to bottom). The default heuristic \
                       infers names from instability — unreliable for non-web codebases. Pass \
                       N names matching `num_layers`."
    )]
    pub layer_names: Option<Vec<String>>,
}

/// Tier 3 — `shotgun_surgery_fix` (consolidation recommender for shotgun-surgery smells).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ShotgunSurgeryFixParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Minimum co-change partners for a hub to qualify (default: 6). Mirrors \
                       the threshold used by design_smell_detection."
    )]
    pub min_partners: Option<i32>,
    #[schemars(description = "Minimum Jaccard co-change similarity (default: 0.2).")]
    pub min_coupling: Option<f64>,
    #[schemars(description = "Maximum hubs to return (default: 15)")]
    pub limit: Option<i32>,
}

/// Tier 3 — `fix_circular_dependency` (cycle-breaking edge selection).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FixCircularDependencyParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Maximum cycle length to enumerate per SCC (default: 10). Longer cycles \
                       are reported as the SCC summary only."
    )]
    pub max_cycle_length: Option<i32>,
    #[schemars(description = "Maximum fix candidates to return (default: 20)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "Strategy preference: \"interface\", \"inversion\", or \"auto\" (default). \
                       Auto picks based on Ce/Ca/instability of the cycle nodes."
    )]
    pub prefer_strategy: Option<String>,
}

/// Tier 3 — `recommend_module_split` (split god files using chunk → topic mapping).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecommendModuleSplitParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Minimum file line_count to consider for splitting (default: 500). \
                       Mirrors the god_class threshold used by design_smell_detection."
    )]
    pub min_lines: Option<i32>,
    #[schemars(description = "Maximum split candidates to return (default: 10)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "Minimum number of distinct topic-groups required to recommend a split \
                       (default: 2). Files whose chunks all belong to one dominant topic get an \
                       `add_test` recommendation instead — they're cohesive."
    )]
    pub min_communities: Option<usize>,
    #[schemars(
        description = "If true, include per-chunk membership detail in the output. Default false."
    )]
    pub include_chunks: Option<bool>,
}

/// Tier 3 — `stale_zombie_detector` (graph + history-based dead-code identification).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StaleZombieParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Minimum days since last commit (default: 540 — ~18 months)")]
    pub min_days_idle: Option<i32>,
    #[schemars(
        description = "Maximum PageRank percentile (default: 0.25 — bottom 25%). Files above this \
                       are too central to be zombies."
    )]
    pub max_pagerank_pct: Option<f64>,
    #[schemars(description = "Maximum candidates to return (default: 30)")]
    pub limit: Option<i32>,
}

/// Tier 2 — `boilerplate_clusters` (codegen-worthy near-identical chunks).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BoilerplateClustersParams {
    #[schemars(
        description = "Minimum chunk-pair similarity (default: 0.96). Aggressive — boilerplate \
                       must be near-identical."
    )]
    pub min_similarity: Option<f64>,
    #[schemars(description = "Minimum chunks per cluster (default: 3)")]
    pub min_cluster_size: Option<usize>,
    #[schemars(
        description = "Minimum normalized Jaccard match ratio after identifier substitution \
                       (default: 0.99). Below this, the cluster is real-similarity rather than \
                       boilerplate."
    )]
    pub min_normalized_match: Option<f64>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(description = "Filter pairs touching this project")]
    pub project: Option<String>,
    #[schemars(description = "Maximum clusters to return (default: 20)")]
    pub limit: Option<i32>,
    #[schemars(description = "Worktree filter: \"main\" (default) or \"all\".")]
    pub worktree_filter: Option<String>,
    #[schemars(
        description = "If true, include pairs whose two projects are worktrees of the same \
                       upstream repo. Default false."
    )]
    pub include_same_repo: Option<bool>,
}

/// Tier 2 — `pattern_abstraction_candidates` (trait/interface extraction at medium similarity).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PatternAbstractionParams {
    #[schemars(description = "Minimum chunk-pair similarity (default: 0.70)")]
    pub min_similarity: Option<f64>,
    #[schemars(
        description = "Maximum chunk-pair similarity, exclusive (default: 0.85). Above this is \
                       duplicate code, not pattern."
    )]
    pub max_similarity: Option<f64>,
    #[schemars(
        description = "Minimum FCM topic-membership score on both endpoints (default: 0.55). \
                       Above this means the chunks are confidently in the same topic."
    )]
    pub min_topic_membership: Option<f64>,
    #[schemars(description = "Minimum implementations per pattern candidate (default: 4)")]
    pub min_cluster_size: Option<usize>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(description = "Restrict to pairs touching this project")]
    pub project: Option<String>,
    #[schemars(description = "Maximum candidates to return (default: 20)")]
    pub limit: Option<i32>,
    #[schemars(description = "Worktree filter: \"main\" (default) or \"all\".")]
    pub worktree_filter: Option<String>,
    #[schemars(
        description = "If true, include candidates whose two projects are worktrees of the same \
                       upstream repo. Default false."
    )]
    pub include_same_repo: Option<bool>,
}

/// Tier 2 — `extraction_candidates` (ranked extract-to-shared-crate; superset of refactoring_report).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExtractionCandidatesParams {
    #[schemars(description = "Minimum file-pair similarity (default: 0.85)")]
    pub min_similarity: Option<f64>,
    #[schemars(description = "Minimum projects spanned by a candidate (default: 2)")]
    pub min_projects: Option<usize>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(description = "Maximum candidates to return (default: 20)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "Worktree filter: \"main\" (default) restricts to canonical main projects; \
                       \"all\" includes feature-branch worktrees."
    )]
    pub worktree_filter: Option<String>,
    #[schemars(
        description = "If true, include refactor candidates whose two projects are worktrees / \
                       sibling clones of the same upstream repo. Default false."
    )]
    pub include_same_repo: Option<bool>,
    #[schemars(
        description = "If true (default), count the call sites that would have to update with the \
                       extraction. Set false to skip the extra graph query."
    )]
    pub include_call_sites: Option<bool>,
    #[schemars(
        description = "Risk tier filter: \"any\" (default), \"low\", \"low-med\". Drops candidates \
                       whose risk_tier exceeds the threshold."
    )]
    pub risk_threshold: Option<String>,
}

/// Tier 2 — `internal_dry` (DRY within one file, real-time).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InternalDryParams {
    #[schemars(
        description = "File reference: either `project:relative_path` (e.g. \"f1r3node:src/cli/mod.rs\") or absolute path"
    )]
    pub file: String,
    #[schemars(
        description = "Minimum intra-file chunk-pair similarity (default: 0.80). Lower than \
                       cross-project DRY because semantically related code in the same file \
                       has more shared context."
    )]
    pub min_similarity: Option<f64>,
    #[schemars(
        description = "Minimum chunks per proposed helper (default: 2). Single chunks are \
                       skipped — a helper extracted from one chunk isn't a DRY win."
    )]
    pub min_pairs_per_helper: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TopicHierarchyFcmParams {
    /// Maximum meta-groups to return (default: 50).
    #[schemars(description = "Maximum meta-groups to return (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DiscoverTopicsParams {
    /// Project name for intra-project analysis. Omit for inter-project (cached).
    #[schemars(
        description = "Project name for intra-project analysis. Omit for inter-project (cached global results)."
    )]
    pub project: Option<String>,
    /// Minimum chunks per topic (default: 5)
    #[schemars(description = "Minimum chunks per topic (default: 5)")]
    pub min_cluster_size: Option<i32>,
    /// Filter by programming language
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    /// Maximum topics to return (default: 30)
    #[schemars(description = "Maximum topics to return (default: 30)")]
    pub limit: Option<i32>,
    /// Force recomputation even if cached results exist (default: false)
    #[schemars(description = "Force recomputation even if cached results exist (default: false)")]
    pub refresh: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindOrphansParams {
    /// Project name (optional — all projects if omitted)
    #[schemars(description = "Project name (optional — all projects if omitted)")]
    pub project: Option<String>,
    /// Filter by language
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    /// Max results (default: 50)
    #[schemars(description = "Max results (default: 50)")]
    pub limit: Option<i32>,
    /// "files" for file-level summary, "chunks" for chunk-level detail (default: "files")
    #[schemars(
        description = "\"files\" for file-level summary, \"chunks\" for chunk-level detail (default: \"files\")"
    )]
    pub detail: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindMisplacedCodeParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Minimum mismatch score to report (0.0-1.0, default: 0.5)
    #[schemars(description = "Minimum mismatch score to report (0.0-1.0, default: 0.5)")]
    pub min_mismatch: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindCoupledFilesParams {
    /// Project name (required — needs git history)
    #[schemars(description = "Project name (required — needs git history)")]
    pub project: String,
    /// Minimum Jaccard coupling score (0.0-1.0, default: 0.3)
    #[schemars(description = "Minimum Jaccard coupling score (0.0-1.0, default: 0.3)")]
    pub min_coupling: Option<f64>,
    /// Minimum co-commits to consider (default: 3)
    #[schemars(description = "Minimum co-commits to consider (default: 3)")]
    pub min_commits: Option<i32>,
    /// Max results (default: 50)
    #[schemars(description = "Max results (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TestCoverageGapsParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ComplexityHotspotsParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Max results (default: 20)
    #[schemars(description = "Max results (default: 20)")]
    pub limit: Option<i32>,
    /// Sort by: "composite", "size", "chunks", "topics", "coupling" (default: "composite")
    #[schemars(
        description = "Sort by: \"composite\", \"size\", \"chunks\", \"topics\", \"coupling\" (default: \"composite\")"
    )]
    pub sort_by: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TopicHierarchyParams {
    /// Project name (optional — global if omitted)
    #[schemars(description = "Project name (optional — global if omitted)")]
    pub project: Option<String>,
    /// Number of meta-topic groups to form (default: auto = topics/3)
    #[schemars(description = "Number of meta-topic groups to form (default: auto = topics/3)")]
    pub num_groups: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SuggestMergesParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Filter by language (default: \"markdown\", use \"*\" for all)
    #[schemars(description = "Filter by language (default: \"markdown\", use \"*\" for all)")]
    pub language: Option<String>,
    /// Minimum weighted Jaccard overlap (0.0-1.0, default: 0.4)
    #[schemars(description = "Minimum weighted Jaccard overlap (0.0-1.0, default: 0.4)")]
    pub min_overlap: Option<f64>,
    /// Maximum merge groups to return (default: 20)
    #[schemars(description = "Maximum merge groups to return (default: 20)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SuggestSplitsParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Filter by language (default: \"markdown\", use \"*\" for all)
    #[schemars(description = "Filter by language (default: \"markdown\", use \"*\" for all)")]
    pub language: Option<String>,
    /// Minimum Shannon entropy to flag as split candidate (default: 1.5)
    #[schemars(
        description = "Minimum Shannon entropy of topic distribution to flag (default: 1.5)"
    )]
    pub min_entropy: Option<f64>,
    /// Minimum distinct topics per file to flag (default: 3)
    #[schemars(description = "Minimum distinct topics to flag as split candidate (default: 3)")]
    pub min_topics: Option<i32>,
    /// Maximum results (default: 20)
    #[schemars(description = "Maximum results (default: 20)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocCoverageGapsParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
}

// === Phase 2: Graph Analysis tool parameter types ===

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DependencyGraphParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Focus on a specific file (BFS neighborhood)
    #[schemars(
        description = "Focus on a specific file (BFS neighborhood). Relative path within the project."
    )]
    pub focus_file: Option<String>,
    /// BFS depth when focus_file is set (default: 2)
    #[schemars(description = "BFS depth when focus_file is set (default: 2)")]
    pub depth: Option<i32>,
    /// Edge types to include (default: [\"import\"])
    #[schemars(
        description = "Edge types to include: \"import\", \"co_change\", \"semantic\" (default: [\"import\"])"
    )]
    pub edge_types: Option<Vec<String>>,
    /// Output format: "summary", "edges", "dot" (default: "summary")
    #[schemars(
        description = "Output format: \"summary\" (node/edge counts), \"edges\" (edge list), \"dot\" (Graphviz DOT) (default: \"summary\")"
    )]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CentralityAnalysisParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Centrality metric: "pagerank", "betweenness", "degree", "all" (default: "all")
    #[schemars(
        description = "Centrality metric: \"pagerank\", \"betweenness\", \"degree\", \"all\" (default: \"all\")"
    )]
    pub metric: Option<String>,
    /// Max results (default: 20)
    #[schemars(description = "Max results (default: 20)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CommunityDetectionParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Graph type: "import", "co_change", "combined" (default: "import")
    #[schemars(
        description = "Graph type for community detection: \"import\", \"co_change\", \"combined\" (default: \"import\")"
    )]
    pub graph_type: Option<String>,
    /// Louvain resolution parameter (default: 1.0, higher = more communities)
    #[schemars(
        description = "Louvain resolution parameter (default: 1.0, higher = more communities)"
    )]
    pub resolution: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CircularDependenciesParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Maximum cycle length to report (default: 10)
    #[schemars(description = "Maximum cycle length to report (default: 10)")]
    pub max_cycle_length: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ChangeImpactAnalysisParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// File to analyze impact for (relative path)
    #[schemars(description = "File to analyze impact for (relative path within the project)")]
    pub file: String,
    /// BFS depth for transitive impact (default: 3)
    #[schemars(description = "BFS depth for transitive impact (default: 3)")]
    pub depth: Option<i32>,
    /// Include semantic similarity neighbors (default: true)
    #[schemars(description = "Include semantic similarity neighbors (default: true)")]
    pub include_semantic: Option<bool>,
}

// === Phase 3: Architecture & Design Quality tool parameter types ===

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CouplingCohesionReportParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Directory depth for module grouping (default: 2)
    #[schemars(description = "Directory depth for module grouping (default: 2)")]
    pub module_depth: Option<i32>,
    /// Sort by: "instability", "distance", "coupling", "cohesion" (default: "distance")
    #[schemars(
        description = "Sort by: \"instability\", \"distance\", \"coupling\", \"cohesion\" (default: \"distance\")"
    )]
    pub sort_by: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArchitectureViolationsParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Optional JSON layer configuration for custom architecture rules
    #[schemars(
        description = "Optional JSON layer configuration for custom architecture rules (e.g., {\"layers\": [\"api\", \"service\", \"data\"]})"
    )]
    #[allow(dead_code)]
    pub layer_config: Option<String>,
    /// Minimum severity to report: "low", "medium", "high", "critical" (default: "medium")
    #[schemars(
        description = "Minimum severity to report: \"low\", \"medium\", \"high\", \"critical\" (default: \"medium\")"
    )]
    pub severity_threshold: Option<String>,
    /// Whether to embed a typed `recommended_fix` action on each violation. Default true.
    /// Set false to reproduce the pre-2026-04-30 diagnostic-only output shape.
    #[schemars(
        description = "Whether to embed a typed recommended_fix action on each violation (default: true). \
                       Set false to reproduce the pre-2026-04-30 diagnostic-only shape."
    )]
    pub include_fixes: Option<bool>,
    /// Module path prefixes (relative to project root) to exempt from the
    /// god-module rule. Intentional one-file-per-tool / one-file-per-pattern
    /// catalogs would otherwise be mis-flagged. When omitted, pgmcp's
    /// canonical defaults apply (see `tool_architecture_violations` body).
    #[schemars(
        description = "Module path prefixes to exempt from the god-module rule (e.g. [\"src/patterns\", \"src/mcp/tools\", \"pgmcp-testing/tests\"]). When omitted, pgmcp's canonical defaults apply."
    )]
    pub excluded_god_module_prefixes: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DesignSmellDetectionParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Specific smells to detect (default: all)
    #[schemars(
        description = "Specific smells to detect: \"god_class\", \"srp_violation\", \"shotgun_surgery\", \"stale_module\", \"unstable_dependency\" (default: all)"
    )]
    pub smells: Option<Vec<String>>,
    /// Max results (default: 30)
    #[schemars(description = "Max results (default: 30)")]
    pub limit: Option<i32>,
    /// Whether to embed a typed `recommended_fix` action on each smell. Default true.
    #[schemars(
        description = "Whether to embed a typed recommended_fix action on each smell (default: true). \
                       Set false to reproduce the pre-2026-04-30 diagnostic-only shape."
    )]
    pub include_fixes: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArchitectureQualityParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Detail level: "summary", "full" (default: "summary")
    #[schemars(
        description = "Detail level: \"summary\" (scores only), \"full\" (scores + per-dimension detail) (default: \"summary\")"
    )]
    pub detail: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DesignMetricsParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Scope: "project", "module", "file" (default: "project")
    #[schemars(
        description = "Scope: \"project\" (aggregate), \"module\" (per directory), \"file\" (per file) (default: \"project\")"
    )]
    pub scope: Option<String>,
    /// Path filter for module/file scope
    #[schemars(description = "Path filter for module/file scope (directory prefix or file path)")]
    pub path: Option<String>,
    /// Max results (default: 30)
    #[schemars(description = "Max results (default: 30)")]
    pub limit: Option<i32>,
    /// Sort by: "system_complexity", "cyclomatic", "maintainability", "wmc" (default: "system_complexity")
    #[schemars(
        description = "Sort by: \"system_complexity\", \"cyclomatic\", \"maintainability\", \"wmc\" (default: \"system_complexity\")"
    )]
    pub sort_by: Option<String>,
}

// === Phase 4: ML Prediction tool parameter types (heuristic-based) ===

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BugPredictionParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Max results (default: 20)
    #[schemars(description = "Max results (default: 20)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TechnicalDebtAnalysisParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Max results (default: 30)
    #[schemars(description = "Max results (default: 30)")]
    pub limit: Option<i32>,
    /// Include TODO/FIXME/HACK scan (default: true)
    #[schemars(description = "Include TODO/FIXME/HACK scan (default: true)")]
    pub include_todos: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AnomalyDetectionParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Max anomalies to return (default: 20)
    #[schemars(description = "Max anomalies to return (default: 20)")]
    pub limit: Option<i32>,
    /// Expected contamination ratio (default: 0.05)
    #[schemars(
        description = "Expected contamination ratio, fraction of files expected to be anomalous (default: 0.05)"
    )]
    pub contamination: Option<f64>,
}

// SOTA Phase 2 — graph algorithms (Seidman, Cohen, Tong, Brandes, Burt, Milo, Holme)
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KcoreAnalysisParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Minimum coreness to include (default: 0)")]
    pub min_core: Option<u32>,
    #[schemars(description = "Max files to return (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KtrussAnalysisParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Minimum trussness to include (default: 3)")]
    pub min_truss: Option<u32>,
    #[schemars(description = "Max edges to return (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PersonalizedPagerankParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Seed file paths (required, ≥1)")]
    pub seed_files: Vec<String>,
    #[schemars(description = "Damping factor (default: 0.85)")]
    pub damping: Option<f64>,
    #[schemars(description = "Max files to return (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EdgeBetweennessParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max edges to return (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StructuralHolesParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Sort: \"constraint_asc\" (default, brokers first) or \"constraint_desc\""
    )]
    pub sort: Option<String>,
    #[schemars(description = "Max files to return (default: 30)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MotifCensusParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AttackVulnerabilityParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Removal order: \"pagerank\" (default), \"betweenness\", or \"degree\""
    )]
    pub removal_order: Option<String>,
    #[schemars(description = "Max removal steps (default: 50)")]
    pub max_steps: Option<u32>,
}

// SOTA Phase 3 — information theory
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CompressionDistanceParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "First file path (required)")]
    pub file_a: String,
    #[schemars(description = "Second file path (required)")]
    pub file_b: String,
    #[schemars(description = "zstd compression level (default: 3)")]
    pub level: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CochangeMutualInformationParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Minimum joint commits required to include a pair (default: 3)")]
    pub min_support: Option<u32>,
    #[schemars(description = "Max pairs to return (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ImportEntropyParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Sort: \"entropy_desc\" (default) or \"entropy_asc\"")]
    pub sort: Option<String>,
    #[schemars(description = "Max files to return (default: 30)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IdentifierEntropyParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Sort: \"entropy_desc\" (default) or \"entropy_asc\"")]
    pub sort: Option<String>,
    #[schemars(description = "Max files to return (default: 30)")]
    pub limit: Option<i32>,
}

// SOTA Phase 4 — evolution + quality
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BusFactorParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Fraction of lines that must be unmaintained to count (default: 0.5)"
    )]
    pub threshold: Option<f64>,
    #[schemars(description = "Max files to return (default: 30)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KnowledgeSilosParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Minimum Herfindahl index to include (default: 0.7 = high concentration)"
    )]
    pub min_herfindahl: Option<f64>,
    #[schemars(description = "Max files to return (default: 30)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OwnershipCouplingMismatchParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Minimum Jaccard coupling to include (default: 0.3)")]
    pub min_coupling: Option<f64>,
    #[schemars(description = "Minimum joint commits (default: 3)")]
    pub min_commits: Option<u32>,
    #[schemars(description = "Max pairs to return (default: 30)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocCodeDriftParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Minimum cosine distance to include (default: 0.3)")]
    pub min_drift: Option<f64>,
    #[schemars(description = "Max directories to return (default: 30)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TestSmellsParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max files to return (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MutationScoreSurrogateParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max files to return (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FlakyTestCandidatesParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max test files to return (default: 30)")]
    pub limit: Option<i32>,
}

// SOTA Phase 5 — concurrency / safety / performance
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LocksetRacesParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max matches to return (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UnsafeClustersParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max files to return (default: 25)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PanicPathsParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Entry filter: \"any\" (default), \"pub\", \"module\", \"private\"")]
    pub entry_filter: Option<String>,
    #[schemars(description = "Max functions to return (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CentralFunctionsParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Ranking metric: \"pagerank\" (default), \"betweenness\", \"harmonic\", or \"coreness\""
    )]
    pub metric: Option<String>,
    #[schemars(description = "Max functions to return (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FunctionCommunitiesParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Minimum community size to report (default: 2)")]
    pub min_size: Option<i32>,
    #[schemars(description = "Max communities to return, largest first (default: 30)")]
    pub limit: Option<i32>,
    #[schemars(description = "Max member functions listed per community (default: 15)")]
    pub members_per_community: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FunctionKcoreParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Minimum coreness to report (default: 2)")]
    pub min_coreness: Option<i32>,
    #[schemars(description = "Max functions to return (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecursiveClustersParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max simple-cycle length to enumerate per cluster (default: 8)")]
    pub max_cycle_len: Option<i32>,
    #[schemars(description = "Max recursion clusters to return, largest first (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExtendedCentralityParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Metric: \"eigenvector\" (default), \"katz\", \"harmonic\", \"closeness\", or \"reverse_pagerank\""
    )]
    pub metric: Option<String>,
    #[schemars(
        description = "Graph scope: \"file\" (import graph, default) or \"function\" (call graph)"
    )]
    pub scope: Option<String>,
    #[schemars(description = "Max nodes to return (default: 50)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "Katz attenuation factor alpha (default: 0.1); only used for metric=katz"
    )]
    pub alpha: Option<f64>,
    #[schemars(description = "Katz base constant beta (default: 1.0); only used for metric=katz")]
    pub beta: Option<f64>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArticulationPointsParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Graph scope: \"file\" (import graph, default) or \"function\" (call graph)"
    )]
    pub scope: Option<String>,
    #[schemars(description = "Max cut vertices and bridges to return (default: 100)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GraphConnectivityParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Graph scope: \"file\" (import graph, default) or \"function\" (call graph)"
    )]
    pub scope: Option<String>,
    #[schemars(description = "Max components / partition members to list (default: 20)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CkMetricsParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Sort key: wmc (default) | dit | noc | cbo | rfc")]
    pub sort: Option<String>,
    #[schemars(description = "Max classes to return (default: 40)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SpectralAnalysisParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Graph scope: \"file\" (import graph, default) or \"function\" (call graph)"
    )]
    pub scope: Option<String>,
    #[schemars(description = "WL refinement rounds for structural clones (default: 2, max 6)")]
    pub wl_iterations: Option<i32>,
    #[schemars(description = "Max bisection members / clone classes to list (default: 20)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArchitectureDsmParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Graph scope: \"file\" (import graph, default) or \"function\" (call graph)"
    )]
    pub scope: Option<String>,
    #[schemars(
        description = "Max files per ranked list (top-by-VFI, top-by-VFO, cyclic core); default 20"
    )]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodePprSearchParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Natural-language or code query (required)")]
    pub query: String,
    #[schemars(description = "Number of result files to return (default: 10, max 100)")]
    pub k: Option<i32>,
    #[schemars(description = "Dense seed files to restart PageRank on (default: 10, max 100)")]
    pub max_seeds: Option<i32>,
    #[schemars(description = "PageRank damping/restart factor alpha in [0,1] (default: 0.85)")]
    pub alpha: Option<f64>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodePathSearchParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Natural-language or code query (required)")]
    pub query: String,
    #[schemars(description = "Number of ranked paths to return (default: 15, max 200)")]
    pub k: Option<i32>,
    #[schemars(description = "Dense seed files to start paths from (default: 5, max 50)")]
    pub max_seeds: Option<i32>,
    #[schemars(description = "Maximum edges per path (default: 4, max 6)")]
    pub max_hops: Option<i32>,
    #[schemars(
        description = "Prune a path once its accumulated flow (product of edge weights) drops below \
this; in [0,1], default 0.1"
    )]
    pub min_flow: Option<f64>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodeRaptorSearchParams {
    #[schemars(
        description = "Project name to scope to; omit to search conceptual summaries across ALL projects"
    )]
    pub project: Option<String>,
    #[schemars(description = "Conceptual query (required)")]
    pub query: String,
    #[schemars(description = "Number of summaries to return (default: 10, max 100)")]
    pub k: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HitsParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Graph scope: \"file\" (import graph, default) or \"function\" (call graph)"
    )]
    pub scope: Option<String>,
    #[schemars(description = "Max hubs and authorities to return (default: 25)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DominatorTreeParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Graph scope: \"file\" (import graph, default) or \"function\" (call graph)"
    )]
    pub scope: Option<String>,
    #[schemars(
        description = "Root/entry node (exact label else substring); default = highest-out-degree node"
    )]
    pub root: Option<String>,
    #[schemars(description = "Max chokepoints to return (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeadlockCandidatesParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendSyncViolationsParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max matches to return (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QuadraticLoopsParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max matches to return (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MissingPreallocationParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max matches to return (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BlockingInAsyncParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max matches to return (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CloneDensityParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max files to return (default: 30)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IoHotpathParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max files to return (default: 30)")]
    pub limit: Option<i32>,
}

// SOTA Phase 6 — security
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaintAnalysisParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max findings (default: 30)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SecretDetectionParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Minimum Shannon entropy to flag a literal (default: 4.0)")]
    pub min_entropy: Option<f64>,
    #[schemars(description = "Max findings (default: 100)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CryptoMisuseParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max findings (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UnsafeDeserializationParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max matches (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InjectionCandidatesParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "\"sql\" / \"shell\" / \"all\" (default)")]
    pub kind: Option<String>,
    #[schemars(description = "Max matches (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UnprotectedRoutesParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max matches (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CveSupplyChainParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max dependencies to return (default: 200)")]
    pub limit: Option<i32>,
}

// SOTA Phase 7 — API / contract
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PublicApiSurfaceParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Language filter (e.g. \"rust\"); omit = all")]
    pub language: Option<String>,
    #[schemars(description = "\"summary\" (default) or \"full\"")]
    pub format: Option<String>,
    #[schemars(description = "Max symbols to return when format=\"full\" (default: 500)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SemverBreakAuditParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "How many recent commits to scan for historical public surface (default: 50)"
    )]
    pub window_commits: Option<u32>,
    #[schemars(description = "Max findings (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeprecatedButUsedParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max symbols to return (default: 30)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ApiStabilityParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "How many recent commits to scan (default: 100)")]
    pub window_commits: Option<u32>,
    #[schemars(description = "Max symbols to return (default: 50)")]
    pub limit: Option<i32>,
}

// SOTA Phase 8 — ML / embedding-based
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LshCloneDetectionParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Approximate-cosine threshold (default: 0.85)")]
    pub min_similarity: Option<f64>,
    #[schemars(description = "Max pairs (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SemanticDriftParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max files (default: 30)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EmbeddingOutliersParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Number of nearest neighbours (default: 20)")]
    pub k: Option<u32>,
    #[schemars(description = "LOF threshold (default: 1.5)")]
    pub threshold: Option<f64>,
    #[schemars(description = "Max outliers (default: 30)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MultiResolutionPagerankParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max files (default: 50)")]
    pub limit: Option<i32>,
}

// SOTA Phase 9 — data engineering
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MigrationSafetyParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max findings (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeadColumnsParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max columns (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PiiSpreadParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Scope: \"all\" (default), \"logs\", \"network\"")]
    pub scope: Option<String>,
    #[schemars(description = "Max findings (default: 50)")]
    pub limit: Option<i32>,
}

// SOTA Phase 10 — call-graph downstream
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeadCodeReachabilityParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Include test files as roots (default: false)")]
    pub include_tests: Option<bool>,
    #[schemars(description = "Max dead candidates (default: 50)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "Include bare-name-resolved call edges (resolution_kind = 'bare_name_in_project') \
                       in the reachability walk. Default false: only high-confidence \
                       (exact_in_file / exact_via_import) edges are used, which produces a more \
                       precise dead-code report. Set true to inflate the reachable set with \
                       ambiguous-name matches (reduces dead candidates but accepts more noise)."
    )]
    pub include_bare_name: Option<bool>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FeatureEnvyParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "ATFD threshold (default: 0.6)")]
    pub threshold: Option<f64>,
    #[schemars(description = "Max functions (default: 30)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ShotgunSurgeryParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "How many recent commits to scan (default: 50)")]
    pub since_commits: Option<u32>,
    #[schemars(description = "Minimum files touched to count as shotgun (default: 4)")]
    pub min_files: Option<u32>,
    #[schemars(description = "Max commits (default: 30)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Lcom4Params {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max containers (default: 30)")]
    pub limit: Option<i32>,
}

// SOTA Phase 11 — evolution analytics
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RefactorPressureParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Window length in days (default: 180)")]
    pub since_days: Option<u32>,
    #[schemars(description = "Max files (default: 30)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CommitChangepointParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max changepoints (default: 20)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CommitTopicDriftParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Window size (default: 20)")]
    pub window_commits: Option<u32>,
    #[schemars(description = "Max files (default: 30)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReleaseApiStabilityParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max commits (default: 50)")]
    pub limit: Option<i32>,
}

// A2A inter-agent IPC bridge params
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct A2aSendTaskParams {
    #[schemars(description = "Name of a registered peer agent (see a2a_list_agents)")]
    pub target_agent: String,
    #[schemars(description = "Message text to send")]
    pub message: String,
    #[schemars(description = "Optional skill_id to invoke on the peer")]
    pub skill_id: Option<String>,
    #[schemars(
        description = "Optional recursion rounds for iterative refinement (1..=10). \
                       Inspired by Yang et al. 2026 RecursiveMAS Section 5."
    )]
    pub recursion_rounds: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct A2aGetTaskParams {
    #[schemars(description = "Name of a registered peer agent")]
    pub target_agent: String,
    #[schemars(description = "Task UUID returned by a2a_send_task")]
    pub task_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct A2aSubscribeTaskParams {
    #[schemars(description = "Name of a registered peer agent")]
    pub target_agent: String,
    #[schemars(description = "Task UUID to stream events for")]
    pub task_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct A2aCancelTaskParams {
    #[schemars(description = "Name of a registered peer agent")]
    pub target_agent: String,
    #[schemars(description = "Task UUID to cancel")]
    pub task_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct A2aRegisterAgentParams {
    #[schemars(description = "Unique agent name (used as the directory key)")]
    pub name: String,
    #[schemars(description = "Agent's JSON-RPC base URL (e.g. http://localhost:3101/a2a/jsonrpc)")]
    pub url: String,
    #[schemars(description = "Optional version string")]
    pub version: Option<String>,
    #[schemars(description = "Optional description")]
    pub description: Option<String>,
    #[schemars(description = "Optional capabilities JSON object")]
    pub capabilities: Option<serde_json::Value>,
    #[schemars(description = "Optional skills JSON array")]
    pub skills: Option<serde_json::Value>,
    #[schemars(description = "Specialty tags (e.g. [\"search\",\"retrieval\"]). \
                       Used by a2a_find_agents_by_specialty for routing.")]
    pub specialty: Option<Vec<String>>,
    #[schemars(description = "Recommended collaboration role \
                       (e.g. \"Search Specialist\", \"Summarizer\", \"Critic\"). \
                       Used by orchestration patterns.")]
    pub recommended_role: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct A2aListAgentsParams {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct A2aFindAgentsBySpecialtyParams {
    #[schemars(description = "Specialty tags to match (OR-logic: any match wins)")]
    pub specialty: Vec<String>,
    #[schemars(description = "Optional exact-match on recommended_role")]
    pub recommended_role: Option<String>,
    #[schemars(description = "Max results (default 10)")]
    pub limit: Option<usize>,
    #[schemars(
        description = "Optional typed-capability filter: agents must carry ALL of these type tags in their structured capabilities descriptor (AND-logic). Adds a ranked `typed_capability_matches` list to the result."
    )]
    pub required_type_tags: Option<Vec<String>>,
    #[schemars(
        description = "Optional typed-capability filter: agents must carry ALL of these effects (e.g. \"network\", \"database\") in their structured capabilities descriptor (AND-logic)."
    )]
    pub required_effects: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct A2aPatternSequentialParams {
    #[schemars(description = "Registered peer name for the Planner role")]
    pub planner_agent: String,
    #[schemars(description = "Registered peer name for the Critic role")]
    pub critic_agent: String,
    #[schemars(description = "Registered peer name for the Solver role")]
    pub solver_agent: String,
    #[schemars(description = "User query")]
    pub message: String,
    #[schemars(description = "Optional outer-loop recursion over the trio (1..=5)")]
    pub recursion_rounds: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct A2aPatternMixtureParams {
    #[schemars(description = "Registered peer names for domain specialists (2..=8)")]
    pub specialist_agents: Vec<String>,
    #[schemars(description = "Registered peer name for the Summarizer role")]
    pub summarizer_agent: String,
    #[schemars(description = "User query (sent to every specialist in parallel)")]
    pub message: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct A2aPatternDistillationParams {
    #[schemars(description = "Registered peer name for the Expert role")]
    pub expert_agent: String,
    #[schemars(description = "Registered peer name for the Learner role")]
    pub learner_agent: String,
    #[schemars(description = "User query")]
    pub message: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct A2aPatternDeliberationParams {
    #[schemars(description = "Registered peer name for the Reflector role")]
    pub reflector_agent: String,
    #[schemars(description = "Registered peer name for the Tool-Caller role")]
    pub tool_caller_agent: String,
    #[schemars(description = "User query")]
    pub message: String,
    #[schemars(description = "Max deliberation rounds (default 3, hard cap 10)")]
    pub max_rounds: Option<u32>,
}

// ── CSM / MPST coordination observer tools (ADR-009) ──────────────────────────
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CsmListProtocolsParams {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CsmProtocolOfPatternParams {
    #[schemars(
        description = "Pattern name or a2a skill_id (\"deliberation\" or \"a2a_pattern_deliberation\")"
    )]
    pub pattern: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CsmShowProjectionParams {
    #[schemars(description = "Pattern name or a2a skill_id")]
    pub protocol: String,
    #[schemars(
        description = "Optional role to show (e.g. \"O\", \"R\", \"T\"); omit for all roles"
    )]
    pub role: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CsmValidateRunParams {
    #[schemars(description = "The a2a_tasks UUID of a completed a2a_pattern_* run")]
    pub task_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CsmProtocolPlanParams {
    #[schemars(description = "Pattern name or a2a skill_id")]
    pub pattern: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CsmInferPeerFsmParams {
    #[schemars(description = "Pattern name or a2a skill_id whose recorded runs to infer from")]
    pub protocol: String,
    #[schemars(description = "Minimum observed runs required to infer (default 1)")]
    pub min_support: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct A2aReportOutcomeParams {
    #[schemars(
        description = "Kind of task this is about, e.g. \"rust-collections\" or \"a2a_pattern_sequential:Solver\""
    )]
    pub task_kind: String,
    #[schemars(description = "Short imperative approach, e.g. \"preallocate Vec with capacity\"")]
    pub approach: String,
    #[schemars(
        description = "Outcome: worked | failed | mixed | prefer | avoid | superseded_by_peer"
    )]
    pub outcome: String,
    #[schemars(description = "Confidence in [0,1] (default 0.6)")]
    pub confidence: Option<f32>,
    #[schemars(description = "Optional supporting snippet / rationale")]
    pub evidence: Option<String>,
    #[schemars(description = "Owning project id; omit for a workspace-general practice")]
    pub project_id: Option<i32>,
    #[schemars(description = "Reporting agent id; defaults to the MCP client name")]
    pub agent_id: Option<String>,
}

// ── Scientific-experiment subsystem ─────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExperimentOpenParams {
    #[schemars(description = "Short experiment title (also the ledger filename stem)")]
    pub title: String,
    #[schemars(description = "The observation/question driving the experiment")]
    pub question: String,
    #[schemars(description = "Problem statement / reproduction / motivation")]
    pub context: Option<String>,
    #[schemars(
        description = "Kind: optimization | feature_refactor | feature_addition | bugfix | investigation | other (default other)"
    )]
    pub kind: Option<String>,
    #[schemars(description = "Owning project id; omit for a workspace-general experiment")]
    pub project_id: Option<i32>,
    #[schemars(description = "The first hypothesis statement (testable prediction)")]
    pub hypothesis: String,
    #[schemars(description = "Primary metric name, e.g. \"p99_latency_ms\", \"lcom4\"")]
    pub primary_metric: String,
    #[schemars(description = "Metric unit, e.g. \"ms\", \"MiB\", \"qps\"")]
    pub unit: Option<String>,
    #[schemars(description = "Predicted effect direction: increase | decrease | either | none")]
    pub predicted_direction: Option<String>,
    #[schemars(
        description = "For the default criterion's tail when none is supplied: true ⇒ lower metric is better (default true)"
    )]
    pub lower_is_better: Option<bool>,
    #[schemars(
        description = "Pre-registered acceptance criterion as JSON (e.g. {\"type\":\"welch_t\",\"alpha\":0.05,\"tail\":\"less\",\"min_effect\":{\"kind\":\"cohens_d\",\"threshold\":0.5}}). Omit for the kind default."
    )]
    pub acceptance_criterion: Option<serde_json::Value>,
    #[schemars(
        description = "Expected standardized effect (Cohen's d) for power-based sample sizing"
    )]
    pub expected_effect: Option<f64>,
    #[schemars(description = "Hardware descriptor JSON {host, gpu, cpu, ram_gb, os}")]
    pub hardware: Option<serde_json::Value>,
    #[schemars(description = "Git commit/branch at open time")]
    pub git_ref: Option<String>,
    #[schemars(description = "Plan / ADR reference path")]
    pub plan_ref: Option<String>,
    #[schemars(description = "Explicit slug; auto-derived from title when omitted")]
    pub slug: Option<String>,
    #[schemars(
        description = "Workspace/relative paths to anchor this experiment to (code it concerns)"
    )]
    pub anchor_paths: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExperimentProtocolParams {
    #[schemars(description = "Experiment id (or use slug)")]
    pub experiment_id: Option<i64>,
    #[schemars(description = "Experiment slug (or use experiment_id)")]
    pub slug: Option<String>,
    #[schemars(description = "Hypothesis id; defaults to the experiment's first hypothesis")]
    pub hypothesis_id: Option<i64>,
    #[schemars(description = "Refined expected effect (Cohen's d) to re-size the sample")]
    pub expected_effect: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExperimentRecordMeasurementParams {
    #[schemars(description = "Experiment id")]
    pub experiment_id: i64,
    #[schemars(description = "Hypothesis id this measurement is for (recommended)")]
    pub hypothesis_id: Option<i64>,
    #[schemars(description = "Arm label, e.g. \"control\" | \"treatment\" | a free label")]
    pub arm_label: String,
    #[schemars(description = "Arm kind: control | treatment | baseline")]
    pub arm_kind: String,
    #[schemars(
        description = "Metric name (matches the hypothesis's primary_metric or a secondary)"
    )]
    pub metric: String,
    #[schemars(description = "Metric unit")]
    pub unit: Option<String>,
    #[schemars(description = "Raw per-replicate (or per-unit) sample values")]
    pub samples: Vec<f64>,
    #[schemars(
        description = "Per-sample keys (e.g. file paths) for paired tests; must align 1:1 with samples"
    )]
    pub unit_keys: Option<Vec<String>>,
    #[schemars(description = "Mark these as warm-up samples (excluded from the test)")]
    pub is_warmup: Option<bool>,
    #[schemars(
        description = "Metric source: external_benchmark | pgmcp_metric | agent_scalar | manual (default manual)"
    )]
    pub source: Option<String>,
    #[schemars(
        description = "Command spec JSON {cmd,args,env,cwd,warmup,runs} or {tool,args,ref}"
    )]
    pub command_spec: Option<serde_json::Value>,
    #[schemars(description = "Run plan JSON (replicates, warmup, pinning, …)")]
    pub run_plan: Option<serde_json::Value>,
    #[schemars(description = "Host metadata JSON (hardware, governor, pinned cores, env)")]
    pub host_meta: Option<serde_json::Value>,
    #[schemars(description = "Git ref this arm was measured at")]
    pub git_ref: Option<String>,
    #[schemars(description = "RNG seed used (for reproducibility)")]
    pub seed: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExperimentDecideParams {
    #[schemars(description = "Hypothesis id to decide")]
    pub hypothesis_id: i64,
    #[schemars(description = "Metric to test; defaults to the hypothesis's primary_metric")]
    pub metric: Option<String>,
    #[schemars(description = "Control arm label (default \"control\")")]
    pub control_arm: Option<String>,
    #[schemars(description = "Treatment arm label (default \"treatment\")")]
    pub treatment_arm: Option<String>,
    #[schemars(description = "Decider id (agent/operator)")]
    pub decided_by: Option<String>,
    #[schemars(description = "Operator prose appended to the auto-generated rationale")]
    pub rationale_note: Option<String>,
    #[schemars(
        description = "Emit a linked agent_outcomes row on accept/reject (consensus→mandate pipeline). Default true."
    )]
    pub link_outcome: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExperimentSearchParams {
    #[schemars(description = "Natural-language query, e.g. \"arena allocation on the hot path\"")]
    pub query: String,
    #[schemars(description = "Restrict to a project id; omit for CROSS-PROJECT recall")]
    pub project_id: Option<i32>,
    #[schemars(description = "Filter by kind (optimization | feature_refactor | …)")]
    pub kind: Option<String>,
    #[schemars(
        description = "Filter by a hypothesis verdict (accepted | rejected | inconclusive)"
    )]
    pub verdict: Option<String>,
    #[schemars(description = "Max results (default 20, max 100)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExperimentGetParams {
    #[schemars(description = "Experiment id (or use slug)")]
    pub experiment_id: Option<i64>,
    #[schemars(description = "Experiment slug (or use experiment_id)")]
    pub slug: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExperimentListParams {
    #[schemars(description = "Filter by project id")]
    pub project_id: Option<i32>,
    #[schemars(description = "Filter by kind")]
    pub kind: Option<String>,
    #[schemars(
        description = "Filter by status (open | measuring | decided | abandoned | superseded)"
    )]
    pub status: Option<String>,
    #[schemars(description = "Max rows (default 50, max 500)")]
    pub limit: Option<i32>,
    #[schemars(description = "Offset for pagination (default 0)")]
    pub offset: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExperimentTimelineParams {
    #[schemars(description = "Experiment id (or use slug)")]
    pub experiment_id: Option<i64>,
    #[schemars(description = "Experiment slug (or use experiment_id)")]
    pub slug: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExperimentLogArtifactParams {
    #[schemars(description = "Tie to a formal experiment (omit for an ad-hoc capture)")]
    pub experiment_id: Option<i64>,
    #[schemars(description = "Project id for an ad-hoc (experiment-less) artifact")]
    pub project_id: Option<i32>,
    #[schemars(
        description = "Artifact kind: perf | hyperfine | criterion | massif | flamegraph | log"
    )]
    pub kind: String,
    #[schemars(description = "Tool that produced it, e.g. \"hyperfine\", \"valgrind\"")]
    pub tool: Option<String>,
    #[schemars(description = "Short label")]
    pub label: Option<String>,
    #[schemars(
        description = "The captured text (perf report, hyperfine JSON, folded stacks, log…)"
    )]
    pub content: Option<String>,
    #[schemars(description = "Pre-parsed metrics JSON (merged with auto-parsed ones)")]
    pub metrics: Option<serde_json::Value>,
    #[schemars(
        description = "Link to an indexed file id if the artifact is also a committed file"
    )]
    pub file_id: Option<i64>,
    #[schemars(description = "Git ref the artifact was captured at")]
    pub git_ref: Option<String>,
    #[schemars(
        description = "Auto-parse known formats (hyperfine/criterion) into a metrics summary (default false)"
    )]
    pub parse: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExperimentRenderLedgerParams {
    #[schemars(description = "Experiment id (or use slug)")]
    pub experiment_id: Option<i64>,
    #[schemars(description = "Experiment slug (or use experiment_id)")]
    pub slug: Option<String>,
    #[schemars(
        description = "Render and RETURN the markdown without writing the file (default false → writes under [experiments] ledger_dir relative to cwd)"
    )]
    pub dry_run: Option<bool>,
}

// ── Work-item / plan tracker subsystem ──────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemCreateParams {
    #[schemars(
        description = "Item kind: plan | goal | epic | task | sub_task | todo | fixme | idea | note | question | nice_to_have | action_item | experiment"
    )]
    pub kind: String,
    #[schemars(description = "Short, human-legible title (also the public_id slug stem)")]
    pub title: String,
    #[schemars(description = "Optional longer description / body")]
    pub body: Option<String>,
    #[schemars(description = "public_id of the parent item (omit for a root)")]
    pub parent_public_id: Option<String>,
    #[schemars(description = "Project name to scope the item to (omit for workspace-global)")]
    pub project: Option<String>,
    #[schemars(description = "Priority; higher sorts first (default 0)")]
    pub priority: Option<i32>,
    #[schemars(description = "Roll-up weight (default 1.0)")]
    pub weight: Option<f32>,
    #[schemars(description = "Whether this item is a parametric (corpus-expanded) template")]
    pub parametric: Option<bool>,
    #[schemars(description = "Corpus glob/spec for a parametric item")]
    pub parametric_corpus: Option<String>,
    #[schemars(description = "Explicit stable public_id (default: generated from the title slug)")]
    pub public_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemGetParams {
    #[schemars(description = "The item's stable public_id")]
    pub public_id: String,
    #[schemars(description = "Also return the full descendant subtree (default false)")]
    pub include_subtree: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemUpdateParams {
    #[schemars(description = "The item's stable public_id")]
    pub public_id: String,
    #[schemars(description = "New title (omit to keep)")]
    pub title: Option<String>,
    #[schemars(description = "New body (omit to keep)")]
    pub body: Option<String>,
    #[schemars(description = "New priority (omit to keep)")]
    pub priority: Option<i32>,
    #[schemars(description = "New roll-up weight (omit to keep)")]
    pub weight: Option<f32>,
    #[schemars(
        description = "Due date as an RFC3339 timestamp (set); empty string or 'none'/'clear' clears it; omit to keep."
    )]
    pub due_at: Option<String>,
    #[schemars(
        description = "Snooze until an RFC3339 timestamp (hides the item from default lists until then); empty/'none' clears; omit to keep."
    )]
    pub snooze_until: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemListParams {
    #[schemars(description = "Filter by project name")]
    pub project: Option<String>,
    #[schemars(description = "Filter by kind")]
    pub kind: Option<String>,
    #[schemars(description = "Filter by status")]
    pub status: Option<String>,
    #[schemars(description = "Filter by parent public_id (direct children of that item)")]
    pub parent_public_id: Option<String>,
    #[schemars(
        description = "When true, return only overdue items (due_at in the past, not done/cancelled/deferred)."
    )]
    pub overdue: Option<bool>,
    #[schemars(
        description = "When true, include currently-snoozed items (snooze_until in the future). Default false hides them."
    )]
    pub include_snoozed: Option<bool>,
    #[schemars(description = "Max rows (default 50, clamped 1..=1000)")]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemTreeParams {
    #[schemars(description = "public_id of the subtree root")]
    pub public_id: String,
    #[schemars(description = "Max rows to return (default 10000, clamped 1..=100000)")]
    pub max_rows: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemReparentParams {
    #[schemars(description = "public_id of the item to move")]
    pub public_id: String,
    #[schemars(
        description = "public_id of the new parent (omit / null to make the item a root). Rejected if it is the item itself or one of its descendants (cycle)."
    )]
    pub new_parent_public_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemSetStatusParams {
    #[schemars(description = "The item's stable public_id")]
    pub public_id: String,
    #[schemars(
        description = "Target status: pending | ready | in_progress | blocked | claimed_done | verifying | cancelled. (verified/deferred/rejected are NOT agent-reachable.)"
    )]
    pub status: String,
    #[schemars(description = "Optional human-readable reason recorded in the status history")]
    pub reason: Option<String>,
}

// ── Phase 2: tags + progress ────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TagCreateParams {
    #[schemars(
        description = "Human-legible tag name (also the slug stem; the slug is the stable key)"
    )]
    pub name: String,
    #[schemars(description = "Optional longer description of what the tag means")]
    pub description: Option<String>,
    #[schemars(description = "Optional display color (free-form, e.g. 'red' or '#cc0000')")]
    pub color: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TagListParams {
    #[schemars(
        description = "Also include merged (tombstoned) tags (default false = active only)"
    )]
    pub include_merged: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TagMergeParams {
    #[schemars(
        description = "Source tag (slug or label) — its assignments are repointed, then it is tombstoned"
    )]
    pub src: String,
    #[schemars(
        description = "Destination tag (slug or label) that absorbs the source's assignments"
    )]
    pub dst: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TagRenameParams {
    #[schemars(
        description = "The tag's stable slug (or original label; it is slugified for lookup). The slug itself is preserved so references survive."
    )]
    pub slug: String,
    #[schemars(description = "The new human-legible name")]
    pub new_name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemTagParams {
    #[schemars(description = "The item's stable public_id")]
    pub public_id: String,
    #[schemars(description = "Tag names/slugs to attach (each is slugified)")]
    pub tags: Vec<String>,
    #[schemars(
        description = "Create unknown tags on demand (default true). When false, unknown tags are reported under 'skipped'."
    )]
    pub auto_create: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemUntagParams {
    #[schemars(description = "The item's stable public_id")]
    pub public_id: String,
    #[schemars(description = "Tag name/slug to detach (slugified for lookup)")]
    pub tag: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemRecordProgressParams {
    #[schemars(description = "The item's stable public_id")]
    pub public_id: String,
    #[schemars(description = "Free-text progress note (required, non-empty)")]
    pub note: String,
    #[schemars(
        description = "Optional self-reported overall percent (0..=100); updates the item's claimed_percent. NOT trusted for the verified roll-up."
    )]
    pub percent: Option<i32>,
    #[schemars(
        description = "Optional agent identity attributed to this progress note (defaults to the calling client's name). Recorded as the progress row's actor_id so the activity feed can attribute it; provenance stays 'agent_write' (NOT trusted for the verified roll-up)."
    )]
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemProgressLogParams {
    #[schemars(description = "The item's stable public_id")]
    pub public_id: String,
    #[schemars(description = "Max notes to return, newest first (default 50, clamped 1..=500)")]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemCompletionParams {
    #[schemars(description = "The root item's stable public_id; rolls up its whole subtree")]
    pub public_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemReprioritizeParams {
    #[schemars(description = "Restrict to a project by name (omit = workspace-wide)")]
    pub project: Option<String>,
    #[schemars(description = "Recency half-life in days for the score (default 14)")]
    pub half_life_days: Option<f64>,
    #[schemars(
        description = "How many top items in the now/next/later plan (default 30, max 500)"
    )]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemSearchParams {
    #[schemars(
        description = "Natural-language query; matched semantically against item title+body"
    )]
    pub query: String,
    #[schemars(description = "Restrict to a project by name (omit = workspace-wide)")]
    pub project: Option<String>,
    #[schemars(description = "Max hits (default 10, max 100)")]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlanRuleInput {
    #[schemars(
        description = "Rule kind: required_kind | allowed_child_kind | required_child_kind | min_children | max_children | required_field | required_acceptance_criterion | quantifier_requires_corpus | naming_rule | id_rule | max_depth_advice"
    )]
    pub rule_kind: String,
    #[schemars(description = "Item kind the rule constrains (omit = whole plan)")]
    pub applies_to_kind: Option<String>,
    #[schemars(
        description = "Child kind for allowed/required_child_kind (comma-separated whitelist allowed)"
    )]
    pub child_kind: Option<String>,
    #[schemars(description = "Min children (min_children)")]
    pub min_count: Option<i32>,
    #[schemars(description = "Max children (max_children) or max depth (max_depth_advice)")]
    pub max_count: Option<i32>,
    #[schemars(description = "Field for required_field: body | due_at | title")]
    pub field_name: Option<String>,
    #[schemars(description = "Regex for naming_rule / id_rule")]
    pub pattern: Option<String>,
    #[schemars(description = "Severity: error | warn | info (default error)")]
    pub severity: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlanDefineParams {
    #[schemars(description = "Definition title (required)")]
    pub title: String,
    #[schemars(description = "Stable slug (defaults to slugified title)")]
    pub slug: Option<String>,
    #[schemars(
        description = "Version (default 1); re-defining a (slug,version) replaces its rules"
    )]
    pub version: Option<i32>,
    #[schemars(description = "Description")]
    pub description: Option<String>,
    #[schemars(description = "Slug of a definition this one extends (inheritance)")]
    pub extends_slug: Option<String>,
    #[schemars(description = "Status: draft | active | deprecated (default active)")]
    pub status: Option<String>,
    #[schemars(description = "The dictated structural rules")]
    pub rules: Vec<PlanRuleInput>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlanValidateParams {
    #[schemars(description = "Root item public_id of the plan instance to validate")]
    pub root_public_id: String,
    #[schemars(description = "Definition slug to validate against")]
    pub definition_slug: String,
    #[schemars(description = "Definition version (omit = latest)")]
    pub definition_version: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlanDefinitionExportParams {
    #[schemars(description = "Definition slug to export")]
    pub slug: String,
    #[schemars(description = "Definition version (omit = latest)")]
    pub version: Option<i32>,
    #[schemars(
        description = "Optional file path to also write the TOML to (parent dirs created). The TOML string is always returned regardless."
    )]
    pub path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlanDefinitionImportParams {
    #[schemars(
        description = "Inline serene-eclipse-shaped TOML ([definition] + optional [scope] + [[rule]]). Provide this OR path."
    )]
    pub toml: Option<String>,
    #[schemars(description = "Path to a TOML file to read. Provide this OR toml.")]
    pub path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemAddCriterionParams {
    #[schemars(description = "The item's public_id")]
    pub public_id: String,
    #[schemars(
        description = "Criterion kind: test | build | lint | proof | model_check | smt | script | auditor_verdict | manual_user_signoff | experiment_verdict"
    )]
    pub criterion_kind: String,
    #[schemars(description = "Human description of what must hold")]
    pub description: String,
    #[schemars(
        description = "Acceptance URI, e.g. cargo://path::test | lean://f.lean::thm | shell://script.sh | auditor://gamma | experiment://slug"
    )]
    pub acceptance_uri: Option<String>,
    #[schemars(description = "Required exit code for shell/cargo/build criteria (default 0)")]
    pub expect_exit: Option<i32>,
    #[schemars(
        description = "Coverage mode: single | universal (universal must cover the full corpus)"
    )]
    pub coverage_mode: Option<String>,
    #[schemars(
        description = "Deferred Stop-hook gate owner: alpha_antistub | beta_verify | gamma_audit | formal (omit normally)"
    )]
    pub gate: Option<String>,
    #[schemars(description = "Whether this criterion is required for verification (default true)")]
    pub required: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemRecordEvidenceParams {
    #[schemars(description = "The acceptance_criteria id this evidence is for")]
    pub criterion_id: i64,
    #[schemars(description = "Verdict: pass | fail | unknown | error")]
    pub verdict: String,
    #[schemars(description = "Exit code (for command/test criteria)")]
    pub exit_code: Option<i32>,
    #[schemars(description = "For universal criteria: how many corpus cases passed")]
    pub coverage_count: Option<i32>,
    #[schemars(description = "For universal criteria: corpus size at check time")]
    pub coverage_total: Option<i32>,
    #[schemars(description = "Repo HEAD sha at verification")]
    pub commit_sha: Option<String>,
    #[schemars(description = "Structured verdict detail as a JSON string (default {})")]
    pub detail_json: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemAttemptVerifyParams {
    #[schemars(description = "The item's public_id; tries the gatekeeper →verified transition")]
    pub public_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemDeferParams {
    #[schemars(description = "The item's public_id to defer (skip)")]
    pub public_id: String,
    #[schemars(
        description = "Why it is being deferred (required; recorded in the append-only audit)"
    )]
    pub reason: String,
    #[schemars(
        description = "The tracker user_token (user-authority gate; agents do not have it)"
    )]
    pub user_token: String,
    #[schemars(description = "Who granted the deferral (default 'user')")]
    pub granted_by: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemReinstateParams {
    #[schemars(description = "The item's public_id to reinstate (deferred → in_progress)")]
    pub public_id: String,
    #[schemars(description = "Why it is being reinstated (required)")]
    pub reason: String,
    #[schemars(description = "The tracker user_token (user-authority gate)")]
    pub user_token: String,
    #[schemars(description = "Who granted the reinstatement (default 'user')")]
    pub granted_by: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemIngestPlanParams {
    #[schemars(
        description = "The plan as markdown (headings → plan/epic/task/sub_task; checklists → todos; numbered → sub_tasks; 'acceptance:' lines → criteria). Idempotent on re-ingest."
    )]
    pub plan_markdown: String,
    #[schemars(description = "Project name to scope the items to (omit = workspace-wide)")]
    pub project: Option<String>,
    #[schemars(
        description = "Optional plan definition slug to validate the ingested tree against"
    )]
    pub definition_slug: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemPromoteMarkerParams {
    #[schemars(
        description = "The marker text (e.g. the TODO/FIXME comment) to promote into a tracked item"
    )]
    pub marker_text: String,
    #[schemars(description = "Source file path the marker came from")]
    pub file: Option<String>,
    #[schemars(description = "Line number of the marker")]
    pub line: Option<i64>,
    #[schemars(description = "Item kind (default: inferred fixme/todo from the marker word)")]
    pub kind: Option<String>,
    #[schemars(description = "Project name to scope to")]
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemClaimParams {
    #[schemars(description = "The item's public_id to claim")]
    pub public_id: String,
    #[schemars(
        description = "Lease seconds before the claim auto-expires (default 300; 10..=86400)"
    )]
    pub lease_secs: Option<i64>,
    #[schemars(description = "Claiming agent id (auto-filled from the MCP client name)")]
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemClaimNextParams {
    #[schemars(
        description = "Restrict to the subtree under this plan public_id (omit = workspace-wide)"
    )]
    pub plan_public_id: Option<String>,
    #[schemars(description = "Lease seconds (default 300)")]
    pub lease_secs: Option<i64>,
    #[schemars(description = "Claiming agent id (auto-filled)")]
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemReleaseParams {
    #[schemars(description = "The item's public_id to release")]
    pub public_id: String,
    #[schemars(description = "Releasing agent id (auto-filled); must be the current owner")]
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemHandoffParams {
    #[schemars(description = "The item's public_id to hand off")]
    pub public_id: String,
    #[schemars(description = "The agent id to hand the claim to")]
    pub to_agent: String,
    #[schemars(description = "Lease seconds for the new owner (default 300)")]
    pub lease_secs: Option<i64>,
    #[schemars(description = "Current owner agent id (auto-filled); must be the current owner")]
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AgentHeartbeatParams {
    #[schemars(description = "Agent id (auto-filled from the MCP client name)")]
    pub agent_id: Option<String>,
    #[schemars(description = "Optionally set the agent's current item public_id")]
    pub current_work_item_public_id: Option<String>,
    #[schemars(description = "Lease seconds to renew the agent's claims to (default 300)")]
    pub lease_secs: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemWhoOwnsParams {
    #[schemars(description = "The item's public_id")]
    pub public_id: String,
    #[schemars(description = "Max claim-history events (default 20)")]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AgentActivityParams {
    #[schemars(
        description = "Agent id to inspect; omit for the active-agent roster ('who is working')"
    )]
    pub agent_id: Option<String>,
    #[schemars(description = "Roster window in seconds (default 600)")]
    pub active_within_secs: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemActivityParams {
    #[schemars(
        description = "Restrict to a plan subtree by its root public_id (omit = workspace-wide)"
    )]
    pub plan_public_id: Option<String>,
    #[schemars(description = "Only events after this RFC3339 timestamp")]
    pub since: Option<String>,
    #[schemars(description = "Max events (default 50, max 500)")]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemLinkParams {
    #[schemars(description = "The source item's public_id (the 'from' end)")]
    pub from_public_id: String,
    #[schemars(description = "The target item's public_id (the 'to' end)")]
    pub to_public_id: String,
    #[schemars(
        description = "Relation type: blocks | depends_on | relates_to | duplicates | supersedes | derived_from. The ordering relations (depends_on/blocks) are rejected if they would create a dependency cycle."
    )]
    pub relation_type: String,
    #[schemars(description = "Optional free-text author attribution for the relation")]
    pub created_by: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemUnlinkParams {
    #[schemars(description = "The source item's public_id")]
    pub from_public_id: String,
    #[schemars(description = "The target item's public_id")]
    pub to_public_id: String,
    #[schemars(description = "Relation type to remove (must match the linked type exactly)")]
    pub relation_type: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemCyclesParams {
    #[schemars(
        description = "Restrict the cycle search to one plan's subtree by its root public_id (only edges with both endpoints in the subtree). Omit for the whole-workspace schedule graph (depends_on + blocks)."
    )]
    pub plan_public_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemAnchorCodeParams {
    #[schemars(description = "The item's public_id to anchor")]
    pub public_id: String,
    #[schemars(
        description = "A file path (project-relative or suffix) to resolve to an indexed file"
    )]
    pub file: Option<String>,
    #[schemars(description = "An explicit file_chunks.id to anchor to")]
    pub chunk_id: Option<i64>,
    #[schemars(description = "An explicit file_symbols.id to anchor to (most precise)")]
    pub symbol_id: Option<i64>,
    #[schemars(description = "Anchor type label (default inferred: symbol > chunk > file)")]
    pub anchor_type: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemBurndownParams {
    #[schemars(description = "Root public_id of the plan to report on")]
    pub plan_public_id: String,
    #[schemars(description = "Velocity window in days (default 14, clamped 1..=365)")]
    pub window_days: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemExportParams {
    #[schemars(description = "Root public_id of the plan subtree to export")]
    pub plan_public_id: String,
    #[schemars(description = "Output format: 'markdown' (default) or 'org'")]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemLinkExperimentParams {
    #[schemars(description = "The experiment's slug to link/track")]
    pub experiment_slug: String,
    #[schemars(
        description = "Existing work_item public_id to link; omit to auto-create a kind='experiment' tracking task from the experiment's title/question."
    )]
    pub work_item_public_id: Option<String>,
    #[schemars(
        description = "Optional hypothesis id to scope the verdict criterion to one hypothesis"
    )]
    pub hypothesis_id: Option<i64>,
    #[schemars(
        description = "Title for the auto-created tracking task (defaults to the experiment's title)"
    )]
    pub title: Option<String>,
    #[schemars(
        description = "Seed an 'experiment_verdict' acceptance criterion so experiment_decide can auto-verify the task (default true)."
    )]
    pub seed_criterion: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct A2aPatternRecursiveParams {
    #[schemars(description = "The long-context question to answer")]
    pub query: String,
    #[schemars(
        description = "Environment handle: {\"kind\":\"file\",\"path\":\"...\"} or {\"kind\":\"corpus\",\"project\":\"...\"}"
    )]
    pub environment: serde_json::Value,
    #[schemars(
        description = "Registered peer name for per-snippet sub-calls (e.g. a Claude/Codex adapter)"
    )]
    pub sub_agent: String,
    #[schemars(description = "Registered peer for the final reduce (defaults to sub_agent)")]
    pub reduce_agent: Option<String>,
    #[schemars(description = "Max snippets to decompose into (1..=64, default 8)")]
    pub max_chunks: Option<usize>,
    #[schemars(description = "Run an extra verification sub-call on the final answer")]
    pub verify: Option<bool>,
    #[schemars(description = "Bounded sub-call concurrency (1..=8, default 4)")]
    pub concurrency: Option<usize>,
    #[schemars(
        description = "Decompose strategy: \"chunk\" | \"semantic\" | \"grep\" (default by environment)"
    )]
    pub strategy: Option<String>,
    #[schemars(
        description = "Max recursion depth (1..=4, default from [a2a.rlm].max_depth). >1 enables true RLM self-recursion over narrowed sub-environments."
    )]
    pub rlm_depth: Option<u32>,
    #[schemars(
        description = "Total sub-call budget across the whole recursion tree (default from [a2a.rlm].max_budget); telescopes across depth so the tree never exceeds it."
    )]
    pub rlm_budget: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TrajectorySimilarityParams {
    #[schemars(
        description = "Probe by an existing RLM run's task_id (uses its recorded trajectory)"
    )]
    pub task_id: Option<String>,
    #[schemars(description = "Or an explicit probe series (encoded step f64s)")]
    pub probe_series: Option<Vec<f64>>,
    #[schemars(description = "Number of nearest trajectories to return (1..=50, default 5)")]
    pub k: Option<usize>,
    #[schemars(
        description = "Re-tune the adaptive MSM cost c from the trajectory set and persist it"
    )]
    pub recalibrate_c: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecognizeTrajectoryParams {
    #[schemars(
        description = "Record type to match against: 'work_item' (progress-% series) or 'file' (weekly churn series)."
    )]
    pub node_type: String,
    #[schemars(
        description = "The partial / in-progress numeric trajectory (ordered f64 samples)."
    )]
    pub series: Vec<f64>,
    #[schemars(description = "Number of nearest references to return (1..=50, default 5).")]
    pub k: Option<i32>,
    #[schemars(description = "MSM split/merge cost c (default 0.1).")]
    pub msm_c: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocumentedTechDebtParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Filter to a single marker kind (e.g. \"TODO\", \"FIXME\"). Omit for all."
    )]
    pub kind: Option<String>,
    #[schemars(description = "Filter by severity: \"high\", \"medium\", \"low\"")]
    pub severity: Option<String>,
    #[schemars(description = "Only markers older than this many days (uses git blame_date)")]
    pub min_age_days: Option<i32>,
    #[schemars(description = "Language filter (e.g. \"rust\")")]
    pub language: Option<String>,
    #[schemars(
        description = "Category: \"comments\", \"stub_macros\", \"deprecated\", or \"all\" (default)"
    )]
    pub category: Option<String>,
    #[schemars(description = "Max findings (default: 100)")]
    pub limit: Option<i32>,
    #[schemars(description = "Output: \"summary\" (default) or \"full\" (per-occurrence list)")]
    pub format: Option<String>,
    /// Glob patterns matched against `f.relative_path`. When omitted,
    /// pgmcp's canonical defaults exclude the curated pattern catalog and
    /// the marker-detector's own test fixtures (so scanning pgmcp itself
    /// doesn't drown in seed prose). `Some(vec![])` disables exclusions.
    #[schemars(
        description = "Glob patterns (relative_path) to exclude from the scan. e.g. [\"src/patterns/**\", \"src/mcp/tools/tool_technical_debt_analysis.rs\"]. When omitted, pgmcp's canonical defaults apply."
    )]
    pub exclude_paths: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TriggerCronParams {
    /// Cron job to run on demand.
    #[schemars(
        description = "Cron job name: \"symbol-extraction\" | \"call-graph\" | \"function-metrics\" | \"fuzzy-sync\" | \"a2a-reflect\" | \"msm-calibrate\". Use symbol-extraction first to populate file_symbols (needed by dead_code_reachability and naming_consistency), then call-graph to populate symbol_references edges, then function-metrics for cyclomatic/cognitive/Halstead/NPath/MI. fuzzy-sync rebuilds the per-project symbol/path/commit/mandate fuzzy tries from PG. Workspace-wide; per-project scoping happens through the project filter on the underlying queries."
    )]
    pub job: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodeOnFireParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Max functions to return (default: 30)
    #[schemars(description = "Max functions to return (default: 30)")]
    pub limit: Option<i32>,
    /// Mode: \"intersect\" (default, churn AND complexity), \"union\" (OR), \"max\" (rank by composite, no filter)
    #[schemars(
        description = "Mode: \"intersect\" (default), \"union\", or \"max\". intersect = churn AND complexity; union = OR; max = no filter, rank by composite score"
    )]
    pub mode: Option<String>,
    /// Churn percentile threshold (default: 0.75 = top quartile)
    #[schemars(description = "Churn percentile threshold (default: 0.75 = top quartile)")]
    pub churn_quartile: Option<f64>,
    /// Cyclomatic percentile threshold (default: 0.75)
    #[schemars(description = "Cyclomatic percentile threshold (default: 0.75)")]
    pub complexity_quartile: Option<f64>,
}

// === Phase 5: NLP & IR tool parameter types ===

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HybridSearchParams {
    /// Search query text
    #[schemars(description = "Search query text")]
    pub query: String,
    /// Filter by project name
    #[schemars(description = "Filter by project name")]
    pub project: Option<String>,
    /// Filter by programming language
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    /// Max results (default: 20)
    #[schemars(description = "Max results (default: 20)")]
    pub limit: Option<i32>,
    /// Weight for BM25/text search (default: 0.5)
    #[schemars(description = "Weight for BM25/text search results (default: 0.5)")]
    pub bm25_weight: Option<f64>,
    /// Weight for semantic search (default: 0.5)
    #[schemars(description = "Weight for semantic search results (default: 0.5)")]
    pub semantic_weight: Option<f64>,
    /// Collapse cross-worktree duplicates
    #[schemars(
        description = "If true, collapse cross-worktree duplicates (see semantic_search). \
                       Default false."
    )]
    pub dedupe_worktrees: Option<bool>,
    /// Weight for the third RRF leg (WFST/HybridLM-rescored query).
    #[schemars(
        description = "Weight for the third RRF leg (WFST lattice + HybridLM-rescored query). \
                       Default 1.0. Set 0.0 to force the legacy 2-leg behavior. The third leg \
                       activates only when the per-project HybridLM model file exists at \
                       <data_dir>/hybrid_lm/<project>/model.bin (populated by the \
                       `ngram-lm-train` cron)."
    )]
    pub wfst_lm_weight: Option<f64>,
    /// Max per-token Damerau-Levenshtein distance for query rewriting.
    #[schemars(
        description = "Max per-token Damerau-Levenshtein distance used when generating \
                       candidates for the third-leg lattice. Default 2."
    )]
    pub max_query_edit_distance: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodeSummarizeParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Scope: "project", "directory", "file" (default: "project")
    #[schemars(
        description = "Scope: \"project\" (whole project overview), \"directory\" (single directory), \"file\" (single file) (default: \"project\")"
    )]
    pub scope: Option<String>,
    /// Path for directory/file scope
    #[schemars(
        description = "Path for directory/file scope (directory prefix or file relative path)"
    )]
    pub path: Option<String>,
    /// Detail level: "brief", "standard", "detailed" (default: "standard")
    #[schemars(
        description = "Detail level: \"brief\", \"standard\", \"detailed\" (default: \"standard\")"
    )]
    pub detail: Option<String>,
}

// === Phase 6: Engineering Scorecard tool parameter types ===

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EngineeringScorecardParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Format: "full", "summary", "failures_only" (default: "full")
    #[schemars(
        description = "Format: \"full\" (all dimensions), \"summary\" (GPA only), \"failures_only\" (grade C or below) (default: \"full\")"
    )]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadFileParams {
    #[schemars(description = "Absolute path of the file to read")]
    pub path: String,
    #[schemars(description = "1-based inclusive start line for a region read. \
                       Combine with `end_line` to fetch only a slice of the \
                       file (stitched from indexed chunks). Use this for long \
                       documents to avoid pulling 20–50k tokens for a paragraph.")]
    pub start_line: Option<i32>,
    #[schemars(
        description = "1-based inclusive end line for a region read. Pair with \
                       `start_line`."
    )]
    pub end_line: Option<i32>,
    #[schemars(description = "Inclusive chunk_index lower bound for a chunk-indexed \
                       region read. Useful when paging large documents.")]
    pub chunk_index_start: Option<i32>,
    #[schemars(description = "Inclusive chunk_index upper bound.")]
    pub chunk_index_end: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectTreeParams {
    #[schemars(description = "Project name")]
    pub project: String,
    #[schemars(description = "Maximum directory depth (default: 5)")]
    pub depth: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OrientParams {
    #[schemars(description = "Project name (as shown by list_projects)")]
    pub project: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct McpToolTelemetryParams {
    #[schemars(
        description = "Filter to a specific MCP tool name (e.g. \"grep\", \"semantic_search\")."
    )]
    pub tool: Option<String>,
    #[schemars(
        description = "Filter to a specific MCP client name (e.g. \"claude-code\", \"cursor\"). Matched case-sensitively against the lowercased name stored in mcp_tool_calls."
    )]
    pub client_name: Option<String>,
    #[schemars(
        description = "Filter to calls that named this project as the `project` parameter."
    )]
    pub project: Option<String>,
    #[schemars(description = "Lookback window in minutes (default 60, max 44640 = 31 days).")]
    pub since_minutes: Option<i32>,
    #[schemars(description = "Result limit for `aggregation=\"raw\"` (default 100, max 1000).")]
    pub limit: Option<i32>,
    #[schemars(
        description = "Aggregation shape: one of `summary`, `top_tools`, `top_callers`, `top_projects`, `error_rate`, `histogram`, `raw`. Default `summary`."
    )]
    pub aggregation: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MandateContextParams {
    #[schemars(
        description = "Project name (as shown by list_projects). Takes precedence over cwd."
    )]
    pub project: Option<String>,
    #[schemars(description = "Working directory used to resolve the nearest indexed project.")]
    pub cwd: Option<String>,
    #[schemars(
        description = "Session UUID. If supplied, response includes active session mandates and any promoted durable mandates for the resolved project."
    )]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionMandatesParams {
    #[schemars(description = "Session UUID. Either session_id or cwd must be supplied.")]
    pub session_id: Option<String>,
    #[schemars(
        description = "Working directory; returns mandates from any session matching this cwd."
    )]
    pub cwd: Option<String>,
    #[schemars(description = "Status filter: 'active' (default), 'all', 'promoted', 'retired'.")]
    pub status: Option<String>,
    #[schemars(description = "Max rows (1..=100, default 20).")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PromoteSessionMandateParams {
    #[schemars(description = "session_mandates.id of the row to promote.")]
    pub mandate_id: i64,
    #[schemars(
        description = "Target scope: 'project' (per-project rule) or 'workspace' (cross-project)."
    )]
    pub scope: String,
    #[schemars(
        description = "Project id to attach the promoted mandate to. Required when scope='project'."
    )]
    pub project_id: Option<i32>,
    #[schemars(
        description = "If true, also append the imperative under a marker section in the appropriate CLAUDE.md / AGENTS.md / .pgmcp.toml. Default false (DB-only)."
    )]
    pub write_to_file: Option<bool>,
    #[schemars(
        description = "Optional explicit file path to write to. If omitted, the handler picks CLAUDE.md / AGENTS.md per scope."
    )]
    pub target_file: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FileInfoParams {
    #[schemars(description = "Absolute path of the file")]
    pub path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecallPromptsParams {
    #[schemars(
        description = "Free-text query — embedded and matched by cosine similarity \
                       against historical prompts in `session_prompts`."
    )]
    pub query: String,
    #[schemars(description = "Optional project filter (matches `projects.name`).")]
    pub project: Option<String>,
    #[schemars(description = "Optional session UUID filter.")]
    pub session: Option<String>,
    #[schemars(description = "Max rows (1..=200, default 10).")]
    pub limit: Option<i32>,
}

// ----------------------------------------------------------------------------
// Memory-server Phase 3.1: official MCP memory-server compatible CRUD Params
// ----------------------------------------------------------------------------

/// Shared scope-filter object accepted by every `memory_*` tool. Each
/// field is optional; missing fields resolve to NULL ("any") on the
/// `memory_scope` row.
#[derive(Debug, Clone, Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct MemoryScopeParam {
    pub user_id: Option<String>,
    pub agent_id: Option<String>,
    /// Optional session UUID (string-encoded).
    pub session_id: Option<String>,
    pub project_id: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryEntityInput {
    #[schemars(description = "Entity name (the unique identifier used by the official server).")]
    pub name: String,
    #[schemars(
        description = "Entity type (free-form string, e.g. 'person', 'project', 'concept')."
    )]
    pub entity_type: String,
    #[schemars(description = "Initial observations attached at create-time. Optional.")]
    pub observations: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryCreateEntitiesParams {
    #[schemars(description = "Entities to create or extend. Idempotent on (name, entity_type).")]
    pub entities: Vec<MemoryEntityInput>,
    #[schemars(
        description = "Scope under which to attach the entities. Defaults to workspace-wide."
    )]
    pub scope: Option<MemoryScopeParam>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryRelationInput {
    pub from: String,
    pub to: String,
    pub relation_type: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryCreateRelationsParams {
    #[schemars(description = "Directed relations between entities. Endpoints must already exist.")]
    pub relations: Vec<MemoryRelationInput>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryObservationInput {
    pub entity_name: String,
    pub contents: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryAddObservationsParams {
    pub observations: Vec<MemoryObservationInput>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryDeleteEntitiesParams {
    pub names: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryObservationDeletionInput {
    pub entity_name: String,
    pub observations: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryDeleteObservationsParams {
    pub deletions: Vec<MemoryObservationDeletionInput>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryDeleteRelationsParams {
    pub relations: Vec<MemoryRelationInput>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryReadGraphParams {
    pub scope: Option<MemoryScopeParam>,
    #[schemars(description = "Max entities returned (default 200, max 2000).")]
    pub limit_entities: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemorySearchNodesParams {
    #[schemars(
        description = "Substring matched against entity name/type/canonical_name and observation content (ILIKE)."
    )]
    pub query: String,
    pub scope: Option<MemoryScopeParam>,
    #[schemars(description = "Max rows (1..=500, default 20).")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryOpenNodesParams {
    pub names: Vec<String>,
}

// ----------------------------------------------------------------------------
// Phase 3.2 pgmcp extensions
// ----------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemorySemanticSearchParams {
    pub query: String,
    pub scope: Option<MemoryScopeParam>,
    #[schemars(
        description = "Optional cognitive-tier filter: working | episodic | semantic | procedural | reflective."
    )]
    pub tier: Option<String>,
    #[schemars(description = "Max rows (1..=200, default 20).")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryHybridSearchParams {
    pub query: String,
    pub scope: Option<MemoryScopeParam>,
    pub tier: Option<String>,
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryFactsAtParams {
    #[schemars(description = "RFC3339 timestamp. Defaults to NOW().")]
    pub as_of: Option<String>,
    pub scope: Option<MemoryScopeParam>,
    pub tier: Option<String>,
    pub limit_entities: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryRelationsTraverseParams {
    pub seed_entity_ids: Vec<i64>,
    #[schemars(description = "BFS depth cap (1..=6, default 2).")]
    pub max_depth: Option<i32>,
    #[schemars(description = "Restrict expansion to one relation_type. Optional.")]
    pub relation_filter: Option<String>,
    #[schemars(description = "Hard cap on total nodes returned (default 200, max 1000).")]
    pub max_nodes: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryAnchorEntityParams {
    pub entity_id: i64,
    pub file_id: Option<i64>,
    pub chunk_id: Option<i64>,
    pub topic_id: Option<i64>,
    #[schemars(description = "Anchor to a file_symbols.id (unified-graph symbol node).")]
    pub symbol_id: Option<i64>,
    #[schemars(description = "Anchor to a projects.id (unified-graph project node).")]
    pub project_id: Option<i32>,
    pub anchor_type: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryUnanchorEntityParams {
    pub anchor_id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryFindCodeForEntityParams {
    pub entity_id: i64,
    pub anchor_type: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryFindEntitiesForCodeParams {
    pub file_id: Option<i64>,
    pub chunk_id: Option<i64>,
    pub topic_id: Option<i64>,
    #[schemars(description = "Find entities anchored to this file_symbols.id.")]
    pub symbol_id: Option<i64>,
    #[schemars(description = "Find entities anchored to this projects.id.")]
    pub project_id: Option<i32>,
}

// ----------------------------------------------------------------------------
// Phase 6 graph-enhanced retrieval Params
// ----------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryUnifiedSearchParams {
    pub query: String,
    #[schemars(
        description = "Optional whitelist of node_types to include (e.g. ['memory_entity','observation','chunk','topic','durable_mandate','commit'])."
    )]
    pub node_types: Option<Vec<String>>,
    pub k: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryNeighborsParams {
    #[schemars(description = "Composite node_id of the seed (e.g. 'memory_entity:42').")]
    pub node_id: String,
    pub depth: Option<i32>,
    pub edge_filter: Option<String>,
    pub max_nodes: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GraphNeighborsParams {
    #[schemars(
        description = "Friendly node reference '<type>:<key>' — key is a natural id (file path, \
project/topic name, work_item public_id, experiment slug, commit sha, symbol name, agent id) or \
a numeric pk. E.g. 'work_item:WI-12', 'file:src/foo.rs', 'project:pgmcp', 'agent:codex', 'chunk:42'."
    )]
    pub node_ref: String,
    #[schemars(description = "Traversal depth (default 1, max 4).")]
    pub depth: Option<i32>,
    #[schemars(description = "Optional edge_type filter (e.g. 'validated_by', 'in_project').")]
    pub edge_filter: Option<String>,
    #[schemars(description = "Hard cap on total nodes returned (default 200, max 500).")]
    pub max_nodes: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryPathSearchParams {
    pub query: String,
    pub seed_node_types: Option<Vec<String>>,
    pub target_node_types: Option<Vec<String>>,
    pub max_hops: Option<i32>,
    pub k: Option<i32>,
    #[schemars(description = "PathRAG prune threshold; paths with Jaccard ≥ this are pruned.")]
    pub prune_jaccard: Option<f32>,
    #[schemars(
        description = "Stage 5b: as-of point-in-time (RFC3339, e.g. '2026-01-01T00:00:00Z') — \
restrict traversal to edges valid at that time (the graph as it was). Omit for the current graph."
    )]
    pub as_of: Option<String>,
    #[schemars(
        description = "Stage 5b: recency half-life in days (default 90) — recent edges are \
up-weighted in path scoring; timeless structural edges are never decayed."
    )]
    pub half_life_days: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryPprSearchParams {
    pub query: String,
    pub k: Option<i32>,
    #[schemars(description = "PageRank teleport probability (default 0.85).")]
    pub alpha: Option<f64>,
    pub max_seeds: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryRaptorSearchParams {
    pub query: String,
    pub scope_id: Option<i64>,
    #[schemars(
        description = "Optional tree-level filter. Level 0 = leaves; level k = k-th summary tier."
    )]
    pub levels: Option<Vec<i32>>,
    pub k: Option<i32>,
}

// ----------------------------------------------------------------------------
// Phase 10 client-profile introspection Params
// ----------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PgmcpClientProfileParams {
    #[schemars(
        description = "Client name to look up (case-insensitive). Defaults to 'generic' when omitted. Match against MCP `clientInfo.name`."
    )]
    pub client_name: Option<String>,
    #[schemars(
        description = "When true, return every registered profile instead of resolving one client name. Default false."
    )]
    pub list_all: Option<bool>,
}

// ----------------------------------------------------------------------------
// Phase 8 forget Params
// ----------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryForgetParams {
    #[schemars(description = "Target row type: 'entity' | 'observation' | 'relation'.")]
    pub target_type: String,
    pub target_id: i64,
    #[schemars(
        description = "When true, hard-delete the row + every dependent FK row and write an audit manifest. \
                       Default false (soft-delete via valid_to)."
    )]
    pub cascade: Option<bool>,
    #[schemars(description = "Actor label written to memory_forget_log (default 'agent').")]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryPurgeExpiredParams {
    pub window_days: Option<i64>,
    pub importance_threshold: Option<f32>,
    #[schemars(description = "When true (default), report counts only — do not delete.")]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryReflectParams {
    pub scope: Option<MemoryScopeParam>,
    #[schemars(
        description = "Optional session UUID — stamps the source on reflection-emitted observations."
    )]
    pub session_id: Option<String>,
    #[schemars(description = "RFC3339 lower-bound on observation creation time. Optional.")]
    pub since: Option<String>,
    #[schemars(
        description = "Max observations to consider in the reflection window. Default 200."
    )]
    pub max_observations: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchMandatesParams {
    #[schemars(description = "Free-text search query — full-text matched against \
                       `imperative || target` in `durable_mandates`.")]
    pub query: String,
    #[schemars(
        description = "Optional polarity filter (one of: always, never, prefer, avoid, \
                       remember, from_now_on, correction, permission, constraint, mandate, \
                       process_rule, project_rule)."
    )]
    pub polarity: Option<String>,
    #[schemars(description = "Optional scope filter ('project' or 'workspace').")]
    pub scope: Option<String>,
    #[schemars(
        description = "Optional project_id filter. Workspace-scoped mandates are always \
                       returned regardless of this filter."
    )]
    pub project_id: Option<i32>,
    #[schemars(description = "Max rows (1..=200, default 20).")]
    pub limit: Option<i32>,
}

// ============================================================================
// Phase D2b — new tool params (6 new MCP tools)
// ============================================================================

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CrossLanguageApiEquivalentsParams {
    #[schemars(description = "Minimum similarity (0.0..=1.0, default 0.7).")]
    pub min_similarity: Option<f32>,
    #[schemars(description = "Maximum number of pairs to return (default 50).")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TypeShapeSearchParams {
    #[schemars(description = "Project name.")]
    pub project: String,
    #[schemars(description = "Required tags in return_type_tags (subset semantics).")]
    pub return_type_tags: Option<Vec<String>>,
    #[schemars(description = "Required tags in any parameter's type_tags (subset semantics).")]
    pub parameter_type_tags: Option<Vec<String>>,
    #[schemars(description = "Required effects (any of).")]
    pub effects: Option<Vec<String>>,
    #[schemars(description = "Maximum matches to return (default 50).")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindCallersBySignatureParams {
    #[schemars(description = "Project name.")]
    pub project: String,
    #[schemars(description = "Resolved target path (e.g. crate::auth::validate).")]
    pub target_path: String,
    #[schemars(description = "Filter callers by parameter type-tag intersection.")]
    pub parameter_type_tags: Option<Vec<String>>,
    #[schemars(description = "Restrict the type-tag filter to a specific parameter position.")]
    pub parameter_position: Option<i32>,
    #[schemars(description = "Filter callers by their own effects (any of).")]
    pub caller_effects: Option<Vec<String>>,
    #[schemars(description = "Maximum callers to return (default 50).")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EffectPropagationParams {
    #[schemars(description = "Project name.")]
    pub project: String,
    #[schemars(description = "Forward mode: BFS reachability from this seed symbol_id.")]
    pub seed_symbol_id: Option<i64>,
    #[schemars(description = "Reverse mode: find symbols that reach any of these effects.")]
    pub target_effects: Vec<String>,
    #[schemars(description = "Maximum BFS depth (1..=32, default 8).")]
    pub max_depth: Option<u32>,
    #[schemars(description = "Maximum results to return (default 50).")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TypeTagDictionaryParams {
    // No filter parameters — this tool is a self-documenting introspection
    // surface for the vocabulary catalogs. The empty struct keeps the
    // JSON-schema shape uniform across tool params.
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SignatureLintParams {
    #[schemars(description = "Project name.")]
    pub project: String,
    #[schemars(description = "Maximum results per finding category (default 50).")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ParadigmProfileParams {
    #[schemars(
        description = "Source code to analyze (raw string). For per-file analysis, the caller \
                       should read the file first and pass its content."
    )]
    pub code: String,
}

// ─────────────────────────────────────────────────────────────────
// Phase 8 — additional MCP tool params (fuzzy + phonetic +
// code-analysis). Each is a thin wrapper over the Phase 4/6/9/10
// helper layers; the tool bodies live in src/mcp/tools/tool_*.rs.
// ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodePropertyGraphParams {
    #[schemars(description = "Source code to build a CPG for.")]
    pub code: String,
    #[schemars(description = "Language identifier (currently: python).")]
    pub language: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SubtreeMiningParams {
    #[schemars(description = "Source-code strings to mine across (same language).")]
    pub sources: Vec<String>,
    #[schemars(description = "Language identifier (python).")]
    pub language: String,
    #[schemars(description = "Min support fraction (0..1, default 0.1).")]
    pub min_support: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PhoneticNormalizeParams {
    #[schemars(description = "String to normalize via liblevenshtein's articulatory framework.")]
    pub term: String,
    /// Optional project name. When set and the project has a
    /// `.pgmcp/rules.llev` override loaded by `event_processor.rs`,
    /// the tool uses that project's rule set instead of the
    /// embedded English default.
    #[schemars(description = "Project name (optional — uses per-project rules if loaded).")]
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExpandQueryToPhoneticPatternParams {
    #[schemars(description = "Query term to reverse-expand into a regex.")]
    pub term: String,
    /// Optional project name. See `PhoneticNormalizeParams.project`.
    #[schemars(description = "Project name (optional — uses per-project rules if loaded).")]
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArticulatoryDistanceParams {
    #[schemars(description = "First string.")]
    pub a: String,
    #[schemars(description = "Second string.")]
    pub b: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DendrogramTopicHierarchyParams {
    #[schemars(description = "Project name.")]
    pub project: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FuzzySymbolSearchParams {
    #[schemars(description = "Query symbol (approximate match).")]
    pub query: String,
    /// Project name (REQUIRED). The persistent symbol trie is
    /// per-project — there is no global view. Callers wanting a
    /// global search should iterate `list_projects` client-side
    /// and merge results.
    #[schemars(
        description = "Project name (required — the persistent symbol trie is per-project)."
    )]
    pub project: String,
    #[schemars(description = "Max edit distance (default 2).")]
    pub max_distance: Option<u32>,
    #[schemars(description = "Result limit (default 20).")]
    pub limit: Option<u32>,
    #[schemars(
        description = "If true, match in phonetic-normalized space (composed phonetic∘edit) instead of raw edit distance. Default false. For a richer phonetic result with kind/visibility, use phonetic_symbol_search."
    )]
    pub phonetic: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FuzzyPathSearchParams {
    #[schemars(description = "Query path fragment (approximate match).")]
    pub query: String,
    /// Project name (REQUIRED). The persistent path trie is
    /// per-project — there is no global view. Callers wanting a
    /// global search should iterate `list_projects` client-side
    /// and merge results.
    #[schemars(description = "Project name (required — the persistent path trie is per-project).")]
    pub project: String,
    #[schemars(description = "Max edit distance (default 2).")]
    pub max_distance: Option<u32>,
    #[schemars(description = "Result limit (default 20).")]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SubstringSearchParams {
    #[schemars(description = "Substring to search for (exact, case-sensitive).")]
    pub needle: String,
    #[schemars(description = "Haystack — list of strings to search within (in-memory)")]
    pub haystack: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TokenGrepParams {
    #[schemars(description = "Query token (matched fuzzily against each haystack token).")]
    pub query: String,
    #[schemars(description = "Haystack tokens.")]
    pub haystack: Vec<String>,
    #[schemars(description = "Max edit distance per token (default 2).")]
    pub max_distance: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TimeSeriesFuzzyMatchParams {
    #[schemars(description = "Probe series (commits per week / similar cadence vector).")]
    pub probe: Vec<f64>,
    #[schemars(description = "Library of candidate series (each with an opaque id).")]
    pub library: Vec<TimeSeriesEntry>,
    #[schemars(description = "K nearest to return (default 5).")]
    pub k: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TimeSeriesEntry {
    pub id: i64,
    pub series: Vec<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CorrectQueryParams {
    #[schemars(description = "User query to correct.")]
    pub query: String,
    #[schemars(
        description = "Project whose persistent symbol vocabulary + n-gram LM drive the correction."
    )]
    pub project: String,
    #[schemars(description = "Max per-token edit distance for candidate generation (default 2).")]
    pub max_distance: Option<u32>,
    #[schemars(
        description = "Language-model interpolation weight 0.0–1.0 (default 0.5; 0 disables LM rescoring)."
    )]
    pub lm_weight: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MandateDedupV2Params {
    #[schemars(description = "Imperative to compare against the candidate set.")]
    pub new_imperative: String,
    #[schemars(description = "Existing mandates as `[id, imperative]` pairs.")]
    pub active: Vec<MandateEntry>,
    #[schemars(description = "Max Damerau-Levenshtein edit distance (default 3).")]
    pub max_distance: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MandateEntry {
    pub id: i64,
    pub imperative: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FuzzyGrepParams {
    #[schemars(description = "Query substring (approximate-match candidate).")]
    pub query: String,
    #[schemars(description = "Haystack strings.")]
    pub haystack: Vec<String>,
    #[schemars(description = "Max edit distance for verification (default 2).")]
    pub max_distance: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PhoneticGrepCommentsParams {
    #[schemars(description = "Query (phonetic-fuzzy match).")]
    pub query: String,
    #[schemars(description = "Haystack lines.")]
    pub haystack: Vec<String>,
    /// Max edit distance allowed on top of phonetic normalization.
    /// Default 1 — tolerates a single character drift after the
    /// rule-set has normalized both sides. Increase to widen the
    /// match envelope; 0 means "exact normalized-form match only".
    #[schemars(description = "Max edit distance on top of phonetic normalization. \
                       Default 1; set 0 for exact normalized match, higher to widen.")]
    pub max_distance: Option<u32>,
    /// Optional project name. See `PhoneticNormalizeParams.project`.
    #[schemars(description = "Project name (optional — uses per-project rules if loaded).")]
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PhoneticSymbolSearchParams {
    #[schemars(description = "Query symbol (composed phonetic∘edit match in normalized space).")]
    pub query: String,
    #[schemars(description = "Project to search — its persistent symbol trie is consulted.")]
    pub project: String,
    #[schemars(description = "Max edit distance in phonetic-normalized space (default 2).")]
    pub max_distance: Option<u32>,
    #[schemars(description = "Maximum number of results (default 20).")]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PhoneticNamingConsistencyParams {
    #[schemars(description = "Identifiers in a directory / class scope to check.")]
    pub identifiers: Vec<String>,
    #[schemars(
        description = "Max articulatory distance to flag as phonetically similar (default: [fuzzy].phonetic_merge_threshold)."
    )]
    pub max_distance: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArticulatoryNamingConsistencyParams {
    #[schemars(description = "Identifiers to compare via articulatory edit distance.")]
    pub identifiers: Vec<String>,
    #[schemars(description = "Max articulatory distance to flag as similar (default 0.5).")]
    pub max_distance: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RenameOracleParams {
    #[schemars(description = "Removed/old symbol name.")]
    pub removed_name: String,
    #[schemars(description = "Candidate current-day names.")]
    pub current_names: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GnnSemanticIssuesParams {
    #[schemars(description = "Source code to scan for semantic issues.")]
    pub code: String,
    #[schemars(description = "Language identifier (currently: python).")]
    pub language: String,
}

#[tool_router]
impl McpServer {
    /// Create a new MCP server from a `SystemContext` bundle.
    pub fn new(ctx: SystemContext) -> Self {
        Self {
            ctx,
            tool_router: Self::tool_router(),
        }
    }

    /// Return the full tool catalog without instantiating an `McpServer`.
    /// Uses the `#[tool_router]` macro's generated `tool_router()` to list all tools.
    pub fn static_tool_catalog() -> Vec<rmcp::model::Tool> {
        Self::tool_router().list_all()
    }

    /// Escape hatch for tool methods + cron-orchestrator calls that still
    /// need a raw `&PgPool` (inline SQL or untraited `crate::db::queries`
    /// callers). Production: returns the underlying pool. With a mock
    /// backend (e.g. `MockDbClient` in tests): panics — those tools are not
    /// reachable through the trait alone and require an integration test
    /// against real Postgres.
    ///
    /// Will be removed in Phase 4 once all such call sites migrate to
    /// trait methods.
    fn pool(&self) -> &PgPool {
        self.db().pool().expect(
            "this MCP tool needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient> \
             or migrate the call site to use a DbClient trait method",
        )
    }

    #[tool(description = "Vector-similarity search across all indexed files. \
USE WHEN: query is conceptual ('error handling patterns', 'auth flow', 'how does X work'), \
cross-project, or you don't know the exact tokens to search for. \
DO NOT USE WHEN: you have an exact symbol/string and just need its locations — `grep` or \
the built-in `Grep` is faster. \
Filter by project name to scope results. Use project: \"claude\" to search past Claude \
Code session transcripts, memory files, and plans from ~/.claude/.")]
    async fn semantic_search(
        &self,
        Parameters(params): Parameters<SemanticSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "semantic_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_semantic_search::tool_semantic_search(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "PostgreSQL full-text search across all indexed files. \
USE WHEN: searching for exact keywords or phrases across multiple projects, with \
ranking by relevance. \
DO NOT USE WHEN: you only need to search the current cwd (built-in `Grep` is faster), \
or when the query is conceptual rather than lexical (use `semantic_search` instead). \
Filter by project; use project: \"claude\" to search Claude Code session transcripts.")]
    async fn text_search(
        &self,
        Parameters(params): Parameters<TextSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "text_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_text_search::tool_text_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Regex pattern search across all indexed files (PostgreSQL ~ operator). \
USE WHEN: searching for a regex across the full indexed codebase or across multiple \
projects, especially when the model has no idea which project the match is in. \
DO NOT USE WHEN: you only need to search within the current cwd or a specific small \
directory tree — the built-in `Grep` tool is faster and respects .gitignore. \
Returns file paths, line numbers, and matching snippets across all indexed projects. \
Set fuzzy=true to match the pattern APPROXIMATELY (liblevenshtein TokenGrep over indexed \
chunks) — finds typo'd / near-miss identifiers exact regex would miss; bound the scan with project."
    )]
    async fn grep(
        &self,
        Parameters(params): Parameters<GrepParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "grep",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_grep::tool_grep(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Read an indexed file by absolute path, returning its content along with \
indexing metadata. \
USE WHEN: reading a file that is part of an indexed project AND you want the metadata \
envelope (last_indexed_at, language, chunk count). \
DO NOT USE WHEN: reading a file you just wrote this turn (not yet indexed), reading a \
.gitignore'd file, or reading a file outside the indexed workspaces — use the built-in \
`Read` tool for those."
    )]
    async fn read_file(
        &self,
        Parameters(params): Parameters<ReadFileParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "read_file",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_read_file::tool_read_file(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "List all discovered projects with file counts.")]
    async fn list_projects(
        &self,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "list_projects",
            30,
            &_ctx,
            "",
            super::tools::tool_list_projects::tool_list_projects(self.ctx()),
        )
        .await
    }

    #[tool(
        description = "Composite first-step orientation snapshot for a project. Bundles project metadata, language breakdown, depth-2 directory tree, key entry points (top files by PageRank), recently-changed files, and top topics into one call. USE WHEN: entering an unfamiliar codebase or starting a non-trivial task — call this before scattering across list_projects/project_tree/centrality_analysis. Returns a `health` envelope flagging stale graph metrics or missing topic data so you can interpret partial results correctly."
    )]
    async fn orient(
        &self,
        Parameters(params): Parameters<OrientParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "orient",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_orient::tool_orient(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Return the effective workspace/project mandate bundle from existing AGENTS.md, CLAUDE.md, and project .pgmcp.toml sources. USE WHEN: starting non-trivial work, checking project rules, or wiring client hooks. MCP surfaces this context advisory-only; hard enforcement still belongs in client hooks, pre-push hooks, CI, or verification scripts. If `session_id` is supplied, the response also includes any active session-scoped mandates and durable mandates promoted for the resolved project."
    )]
    async fn mandate_context(
        &self,
        Parameters(params): Parameters<MandateContextParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "mandate_context",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_mandate_context::tool_mandate_context(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 0: vector-similarity search over historical user \
prompts captured in `session_prompts`. USE WHEN: you want to recall what the user has \
previously asked across sessions ('what have I said about X'), useful for grounding \
agent responses in prior context. Optionally filter by project name or session UUID. \
Returns the top-k most similar prompts with their session id, timestamp, and similarity \
score."
    )]
    async fn recall_prompts(
        &self,
        Parameters(params): Parameters<RecallPromptsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "recall_prompts",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_recall_prompts::tool_recall_prompts(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 0: full-text search over `durable_mandates` \
(promoted standing directives). USE WHEN: you want to look up project rules or \
preferences by keyword. Filters: polarity (always/never/prefer/avoid/...), scope \
('project' or 'workspace'), project_id (workspace-scoped rows are returned regardless). \
Returns mandates ranked by Postgres full-text relevance, then by promotion recency."
    )]
    async fn search_mandates(
        &self,
        Parameters(params): Parameters<SearchMandatesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "search_mandates",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_search_mandates::tool_search_mandates(self.ctx(), params),
        )
        .await
    }

    // ============================================================================
    // Memory-server Phase 3.1: official-compat MCP memory CRUD (9 tools)
    // ============================================================================
    //
    // Drop-in compatible with @modelcontextprotocol/server-memory. Each tool
    // accepts an optional `scope` object {user_id?, agent_id?, session_id?,
    // project_id?} that maps onto `memory_scope`; missing scope = workspace-
    // wide. All deletes are soft-deletes via `valid_to = NOW()` per the
    // bi-temporal contract (decision 3 + decision 7).

    #[tool(
        description = "Memory-server: create entities (knowledge-graph nodes). Drop-in compatible \
with @modelcontextprotocol/server-memory's `create_entities`. Idempotent on \
(name, entity_type): re-use the active row and append observations. Optional \
`scope` attaches the entities to a (user_id, agent_id, session_id, project_id) \
tuple — defaults to workspace-wide."
    )]
    async fn memory_create_entities(
        &self,
        Parameters(params): Parameters<MemoryCreateEntitiesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_create_entities",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_crud::tool_memory_create_entities(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: create directed typed relations between existing entities. \
Drop-in compatible with @modelcontextprotocol/server-memory's `create_relations`. \
Each input `{from, to, relation_type}` is resolved against active entities by \
name; unresolved endpoints return id=-1 in the response."
    )]
    async fn memory_create_relations(
        &self,
        Parameters(params): Parameters<MemoryCreateRelationsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_create_relations",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_crud::tool_memory_create_relations(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: append observations to existing entities. Drop-in \
compatible with @modelcontextprotocol/server-memory's `add_observations`. \
Observations are content-deduped per entity (content_sha256 UNIQUE)."
    )]
    async fn memory_add_observations(
        &self,
        Parameters(params): Parameters<MemoryAddObservationsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_add_observations",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_crud::tool_memory_add_observations(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: soft-delete entities by name (sets valid_to = NOW()). \
Bi-temporal: deleted rows remain queryable via `memory_facts_at(t < deletion_time)`. \
Drop-in compatible with @modelcontextprotocol/server-memory's `delete_entities`."
    )]
    async fn memory_delete_entities(
        &self,
        Parameters(params): Parameters<MemoryDeleteEntitiesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_delete_entities",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_crud::tool_memory_delete_entities(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: soft-delete observations by content text under named \
entities. Drop-in compatible with @modelcontextprotocol/server-memory's \
`delete_observations`."
    )]
    async fn memory_delete_observations(
        &self,
        Parameters(params): Parameters<MemoryDeleteObservationsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_delete_observations",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_crud::tool_memory_delete_observations(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: soft-delete relations by (from, to, relation_type). \
Drop-in compatible with @modelcontextprotocol/server-memory's `delete_relations`."
    )]
    async fn memory_delete_relations(
        &self,
        Parameters(params): Parameters<MemoryDeleteRelationsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_delete_relations",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_crud::tool_memory_delete_relations(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: dump the active knowledge graph (entities + observations + \
relations) under an optional scope. Capped by limit_entities (default 200, max 2000). \
Drop-in compatible with @modelcontextprotocol/server-memory's `read_graph`."
    )]
    async fn memory_read_graph(
        &self,
        Parameters(params): Parameters<MemoryReadGraphParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_read_graph",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_crud::tool_memory_read_graph(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: ILIKE substring search across entity name/type/canonical_name \
and observation content. The Phase 3.1 baseline matching the official server's \
`search_nodes`; the pgmcp-extension `memory_semantic_search` (Phase 3.2, lands \
with BGE-M3 cutover) adds vector similarity."
    )]
    async fn memory_search_nodes(
        &self,
        Parameters(params): Parameters<MemorySearchNodesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_search_nodes",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_crud::tool_memory_search_nodes(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: open named entities — returns each entity plus its active \
observations and incoming/outgoing relations. Drop-in compatible with \
@modelcontextprotocol/server-memory's `open_nodes`."
    )]
    async fn memory_open_nodes(
        &self,
        Parameters(params): Parameters<MemoryOpenNodesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_open_nodes",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_crud::tool_memory_open_nodes(self.ctx(), params),
        )
        .await
    }

    // ============================================================================
    // Memory-server Phase 3.2: pgmcp retrieval extensions (8 tools)
    // ============================================================================

    #[tool(
        description = "Memory-server: BGE-M3 vector search over memory_observations \
(scope/tier filtered). The pgmcp extension to the official-compat `memory_search_nodes` — \
embeds the query with the active embedder and ranks observations by cosine similarity."
    )]
    async fn memory_semantic_search(
        &self,
        Parameters(params): Parameters<MemorySemanticSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_semantic_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_ext::tool_memory_semantic_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: hybrid search over memory_observations — RRF fusion of \
Postgres FTS and BGE-M3 vector cosine, optionally scope/tier filtered."
    )]
    async fn memory_hybrid_search(
        &self,
        Parameters(params): Parameters<MemoryHybridSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_hybrid_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_ext::tool_memory_hybrid_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: bi-temporal point-in-time snapshot. Returns the entities, \
observations, and relations that were active at `as_of` (RFC3339; defaults to NOW())."
    )]
    async fn memory_facts_at(
        &self,
        Parameters(params): Parameters<MemoryFactsAtParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_facts_at",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_ext::tool_memory_facts_at(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: depth-bounded BFS over memory_relations starting from \
one or more seed entity ids. Capped by max_depth (1..=6, default 2) and max_nodes (default \
200, max 1000)."
    )]
    async fn memory_relations_traverse(
        &self,
        Parameters(params): Parameters<MemoryRelationsTraverseParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_relations_traverse",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_ext::tool_memory_relations_traverse(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: anchor an entity to a (file | chunk | topic) with a typed \
anchor_type ('implements', 'tested-by', 'documented-in', 'caused-by', 'applies-to', ...). \
At least one of file_id, chunk_id, topic_id must be provided."
    )]
    async fn memory_anchor_entity(
        &self,
        Parameters(params): Parameters<MemoryAnchorEntityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_anchor_entity",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_ext::tool_memory_anchor_entity(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Memory-server: delete a code anchor by id.")]
    async fn memory_unanchor_entity(
        &self,
        Parameters(params): Parameters<MemoryUnanchorEntityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_unanchor_entity",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_ext::tool_memory_unanchor_entity(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: list code anchors for an entity, optionally filtered by \
anchor_type."
    )]
    async fn memory_find_code_for_entity(
        &self,
        Parameters(params): Parameters<MemoryFindCodeForEntityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_find_code_for_entity",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_ext::tool_memory_find_code_for_entity(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server: reverse lookup — entities anchored to a code object. \
Pass exactly one of file_id, chunk_id, topic_id."
    )]
    async fn memory_find_entities_for_code(
        &self,
        Parameters(params): Parameters<MemoryFindEntitiesForCodeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_find_entities_for_code",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_ext::tool_memory_find_entities_for_code(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 6.3: vector retrieval over the heterogeneous \
unified-nodes view (memory_entity / observation / chunk / topic / durable_mandate / \
commit). Optionally filter to a subset of node_types."
    )]
    async fn memory_unified_search(
        &self,
        Parameters(params): Parameters<MemoryUnifiedSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_unified_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_graph_rag::tool_memory_unified_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 6.3: BFS over the heterogeneous unified-edge view. \
Returns reachable nodes and the edges that connect them, capped by depth ≤ 4 and \
max_nodes ≤ 500."
    )]
    async fn memory_neighbors(
        &self,
        Parameters(params): Parameters<MemoryNeighborsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_neighbors",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_graph_rag::tool_memory_neighbors(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Unified knowledge-graph BFS by *friendly* node reference. Accepts \
'<type>:<key>' where key is a natural id (file path, project/topic name, work_item public_id, \
experiment slug, commit sha, symbol name, agent id) or a numeric pk; resolves it and traverses \
the heterogeneous graph (depth ≤ 4, max_nodes ≤ 500). Valid types: file, project, work_item, \
experiment, topic, symbol, commit, agent, chunk, observation, memory_entity."
    )]
    async fn graph_neighbors(
        &self,
        Parameters(params): Parameters<GraphNeighborsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "graph_neighbors",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_graph_rag::tool_graph_neighbors(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 6.4: PathRAG-style path retrieval. Embed the query, \
seed top-k unified nodes, BFS-expand within max_hops, score paths, then flow-prune \
paths whose Jaccard overlap with a kept path exceeds prune_jaccard."
    )]
    async fn memory_path_search(
        &self,
        Parameters(params): Parameters<MemoryPathSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_path_search",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_graph_rag::tool_memory_path_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 6.2: HippoRAG-style Personalized PageRank over \
memory_relations. Seeds are the top-k entities by best-observation cosine; PPR runs \
25 iterations with the given alpha (teleport probability)."
    )]
    async fn memory_ppr_search(
        &self,
        Parameters(params): Parameters<MemoryPprSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_ppr_search",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_graph_rag::tool_memory_ppr_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 6.1: RAPTOR summary-tree query. Returns top-k summary \
nodes by cosine over summary_embedding, optionally filtered by tree level."
    )]
    async fn memory_raptor_search(
        &self,
        Parameters(params): Parameters<MemoryRaptorSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_raptor_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_graph_rag::tool_memory_raptor_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 10: resolve or list pgmcp client profiles. Pass \
`client_name` to see how pgmcp will format responses for that client (output_format, \
default_brief, include_provenance, per-tool description_overrides); pass `list_all=true` to \
see every profile pgmcp knows about. Built-in defaults for claude-code, codex, and \
generic ship with the binary; assets/client_profiles.toml overrides them."
    )]
    async fn pgmcp_client_profile(
        &self,
        Parameters(params): Parameters<PgmcpClientProfileParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "pgmcp_client_profile",
            10,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_client_profile::tool_pgmcp_client_profile(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 8.4: forget an entity / observation / relation. \
cascade=false (default) sets valid_to = NOW() (soft delete, queryable via \
memory_facts_at); cascade=true hard-deletes + every dependent FK row and writes an \
audit manifest to memory_forget_log."
    )]
    async fn memory_forget(
        &self,
        Parameters(params): Parameters<MemoryForgetParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_forget",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_forget::tool_memory_forget(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 8.2: report (dry_run=true, default) or perform \
(dry_run=false) the retention purge — hard-deletes soft-deleted, past-window, \
low-importance, non-superseded memory_* rows. Defaults pulled from \
[memory.retention] when not provided."
    )]
    async fn memory_purge_expired(
        &self,
        Parameters(params): Parameters<MemoryPurgeExpiredParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_purge_expired",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_forget::tool_memory_purge_expired(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Memory-server Phase 5: reflection. Pull recent observations from the given \
scope (or workspace-wide), call the LLM extractor's reflect path, persist higher-order \
observations with source='reflection' and derived_from = [obs_ids]. Refuses if the \
extractor is disabled or `[memory.reflection] agent_enabled = false`."
    )]
    async fn memory_reflect(
        &self,
        Parameters(params): Parameters<MemoryReflectParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "memory_reflect",
            // Reflection involves an LLM call; allow up to 120 s before the
            // wrapper times out. The cron path runs without this wrapper.
            120,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_memory_reflect::tool_memory_reflect(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "List session-scoped mandates extracted from prompts via the UserPromptSubmit hook. Provide either session_id (preferred) or cwd. Returns active mandates by default; pass status='all' for history."
    )]
    async fn session_mandates(
        &self,
        Parameters(params): Parameters<SessionMandatesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "session_mandates",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_session_mandates::tool_session_mandates(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Promote a session_mandates row to durable scope. scope='project' requires project_id. Inserts into durable_mandates; if write_to_file=true and target_file is supplied, appends the imperative under a marker section in that file (idempotent)."
    )]
    async fn promote_session_mandate(
        &self,
        Parameters(params): Parameters<PromoteSessionMandateParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "promote_session_mandate",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_session_mandates::tool_promote_session_mandate(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Project file tree limited by depth (depth=2 typical). \
USE WHEN: you want the structural overview of a project without enumerating every file \
yourself via `Glob`. \
DO NOT USE WHEN: you only need to glob within a specific subdirectory — the built-in \
`Glob` tool gives you exact pattern matching against the live filesystem. \
For unfamiliar projects, prefer `orient` which bundles project_tree, top topics, and key \
entry points.")]
    async fn project_tree(
        &self,
        Parameters(params): Parameters<ProjectTreeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "project_tree",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_project_tree::tool_project_tree(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Indexed-file metadata envelope (size, language, line count, \
last_indexed_at, project name, chunk count). \
USE WHEN: you want a quick fingerprint of a file before deciding whether to read it, \
or before semantic_search/grep on it specifically. \
DO NOT USE WHEN: the file is not in the index (e.g., just written, .gitignore'd) — \
use the built-in `Bash: stat` or `Read` instead."
    )]
    async fn file_info(
        &self,
        Parameters(params): Parameters<FileInfoParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "file_info",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_file_info::tool_file_info(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Get overall indexing statistics including file counts, search counts, and pool state."
    )]
    async fn index_stats(
        &self,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "index_stats",
            30,
            &_ctx,
            "",
            super::tools::tool_index_stats::tool_index_stats(self.ctx()),
        )
        .await
    }

    #[tool(
        description = "Query per-call MCP tool telemetry from the durable `mcp_tool_calls` table. \
USE WHEN: you want a historical view of which tools were used (over the last N minutes), how long they took (p50/p95/p99), which agents called them, and which projects they targeted. \
DO NOT USE WHEN: you only need real-time counts — `index_stats` and the `pgmcp://stats` resource already carry the live in-memory snapshot. \
Aggregation modes: `summary` (default; (tool × client × project) breakdown with percentiles), `top_tools`, `top_callers`, `top_projects`, `error_rate`, `histogram` (log-spaced duration bands), `raw` (most-recent rows). \
Default lookback is 60 minutes; pass `since_minutes` up to 44640 (31 days) to widen it."
    )]
    async fn mcp_tool_telemetry(
        &self,
        Parameters(params): Parameters<McpToolTelemetryParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "mcp_tool_telemetry",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_mcp_tool_telemetry::tool_mcp_tool_telemetry(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Trigger a full re-index of all workspaces. Clears the existing index and restarts indexing. Can be invoked as a long-running task."
    )]
    async fn reindex(&self, _ctx: RequestContext<RoleServer>) -> Result<CallToolResult, McpError> {
        // No timeout: reindex can run for minutes on a large workspace.
        // Progress is reported via the MCP task store, not the immediate
        // response — wrapping in 30s would falsely fail every full reindex.
        // Routed through `instrumented_tool_run` (not `instrumented_tool_wrap`)
        // so the central tracing events still fire while skipping `timeout_wrap`.
        let caller = extract_caller(&_ctx);
        let request_id = Some(format!("{:?}", _ctx.id));
        instrumented_tool_run(
            self.stats(),
            "reindex",
            None,
            caller,
            "",
            request_id,
            super::tools::tool_reindex::tool_reindex(self.ctx()),
        )
        .await
    }

    #[tool(
        description = "Pairwise file comparison via chunk-level vector similarity. \
USE WHEN: confirming whether two files implement the same concept, deciding if a candidate \
refactor target is similar enough to merge, or auditing apparent duplicates. \
DO NOT USE WHEN: looking for unknown duplicates — use `find_similar_modules` or \
`find_duplicates` to discover them first. \
Always real-time (no batch dependency). Path syntax: project:relative or absolute. Returns \
overall similarity, chunk alignment, and a human-readable verdict."
    )]
    async fn compare_files(
        &self,
        Parameters(params): Parameters<CompareFilesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "compare_files",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_compare_files::tool_compare_files(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Find files similar to a given one across all indexed projects. \
USE WHEN: looking for cross-project copies of a utility, identifying refactor candidates \
(modules that could share a library), or asking 'has someone else solved this?'. \
DO NOT USE WHEN: comparing two specific files — use `compare_files`. \
Queries the materialized similarity table (populated by periodic batch scan); aggregates \
chunk similarity to file-level avg/max/matching count."
    )]
    async fn find_similar_modules(
        &self,
        Parameters(params): Parameters<FindSimilarModulesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "find_similar_modules",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_find_similar_modules::tool_find_similar_modules(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Cross-project duplicate-code cluster discovery (union-find on similarity \
pairs). \
USE WHEN: looking for refactor opportunities across the user's whole indexed workspace, \
finding redundant utilities to consolidate, or auditing copy-paste violations. \
DO NOT USE WHEN: you already know what you're looking for — use `find_similar_modules` \
with a seed file. \
Filters to clusters spanning min_projects+ distinct projects. Requires the similarity \
batch scan to have run at least once."
    )]
    async fn find_duplicates(
        &self,
        Parameters(params): Parameters<FindDuplicatesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "find_duplicates",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_find_duplicates::tool_find_duplicates(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Generate an actionable refactoring report identifying code that could be extracted into shared libraries. Builds on find_duplicates clustering with richer analysis: suggests crate names from common path segments, estimates shared lines, and ranks by project_count * avg_similarity. Requires the similarity batch scan to have run at least once."
    )]
    async fn refactoring_report(
        &self,
        Parameters(params): Parameters<RefactoringReportParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "refactoring_report",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_refactoring_report::tool_refactoring_report(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Cluster near-duplicate code chunks (not files) across projects and propose a \
shared function name per cluster. \
USE WHEN: looking for fine-grained DRY opportunities — two files might be 90% different but share \
a small embedded utility worth extracting. Distinct from `find_duplicates` (file-level) and \
`refactoring_report` (whole-crate extraction). \
DO NOT USE WHEN: you want library-extraction candidates (use `refactoring_report`) or you have \
a specific seed file (use `find_similar_modules`). \
Each cluster includes a typed `recommended_fix` (extract_function or extract_module) with \
proposed function name, module name, and priority_score = loc_avg × project_count × (chunk_count - 1). \
Reads the materialized similarity table; requires the 6-hour similarity-scan cron to have run."
    )]
    async fn chunk_clusters(
        &self,
        Parameters(params): Parameters<ChunkClustersParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "chunk_clusters",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_chunk_clusters::tool_chunk_clusters(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "DRY within a single file: find intra-file chunk pairs above a similarity \
threshold and propose private-helper extractions. \
USE WHEN: you've opened a file and want to know whether parts of it are repeating themselves — \
e.g. multiple HTTP handlers building the same request envelope. Real-time over the indexed \
chunks; no cron dependency. \
DO NOT USE WHEN: looking for cross-file or cross-project DRY (use `chunk_clusters`). \
Returns clusters of similar chunks, each with a proposed `extract_function` recommended_fix \
(action=extract_function, suggested_name, line ranges). Pass `file` as `project:relative_path` \
or absolute path."
    )]
    async fn internal_dry(
        &self,
        Parameters(params): Parameters<InternalDryParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "internal_dry",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_internal_dry::tool_internal_dry(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Extraction-to-shared-crate candidates with effort + risk + proposed API surface. \
Strict superset of `refactoring_report`. \
USE WHEN: planning a `extract_module` PR — you want to know not just *which* code to extract, but \
*how big* the migration is (loc_to_extract, call_sites_to_update) and *how risky* (high churn? many \
unresolved imports?). Each candidate carries a typed `recommended_fix(action=extract_module)`. \
DO NOT USE WHEN: doing a quick \"what duplicates exist?\" survey — `find_duplicates` or \
`refactoring_report` is faster. \
Reads materialized similarity table; requires the 6-hour similarity-scan cron."
    )]
    async fn extraction_candidates(
        &self,
        Parameters(params): Parameters<ExtractionCandidatesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "extraction_candidates",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_extraction_candidates::tool_extraction_candidates(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Trait / interface / Protocol extraction candidates: chunks at *medium* \
similarity (0.70-0.85) sharing the same topic — different implementations of the same idea. \
USE WHEN: looking for OOP / Rust-trait abstraction opportunities. Distinct from `chunk_clusters` \
(near-duplicates → extract function) and `extraction_candidates` (whole-file → extract crate). \
DO NOT USE WHEN: chunks are nearly identical (use `chunk_clusters`) or you have no topic data \
yet (run `discover_topics` first). \
Each candidate includes a typed `recommended_fix(action=extract_trait|extract_interface)` with \
proposed method name, abstraction kind by language, and a diversity-rewarded priority score \
(higher reward for less-similar implementations of the same topic)."
    )]
    async fn pattern_abstraction_candidates(
        &self,
        Parameters(params): Parameters<PatternAbstractionParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "pattern_abstraction_candidates",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_pattern_abstraction::tool_pattern_abstraction(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Codegen-worthy near-identical chunks: clusters where chunks differ only by \
renamed identifiers — strong macro / generic / template candidates. \
USE WHEN: auditing for boilerplate that should be a `macro_rules!` (Rust), generic (TS/Java), \
or parametric template. Aggressive default threshold (0.96) so only near-identical code surfaces. \
DO NOT USE WHEN: looking for general DRY (use `chunk_clusters`); a 0.88 cluster of \"similar idea\" \
code is not a boilerplate cluster. \
For each cluster, identifiers are normalized to positional placeholders; the differing values are \
reported (so you know which identifiers vary across instances). Recommended fix is always \
`extract_macro`. Reads materialized similarity table; requires the 6-hour similarity-scan cron."
    )]
    async fn boilerplate_clusters(
        &self,
        Parameters(params): Parameters<BoilerplateClustersParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "boilerplate_clusters",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_boilerplate_clusters::tool_boilerplate_clusters(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Likely-dead files based on graph + history evidence: low PageRank percentile \
(bottom 25%), in_degree <= 1, and idle for >= 540 days by default. \
USE WHEN: cleaning up legacy modules during a quarterly audit. Distinct from `find_orphans` \
(which uses topic membership) — this combines graph centrality, importer count, and authorial \
abandonment. \
DO NOT USE WHEN: file_metrics is empty (graph cron hasn't run) — the tool soft-fails with a \
guidance message. \
For files with `in_degree=0`, the recommended_fix is `delete_file`. For `in_degree=1`, the fix \
is `move_function` (relocate the single referenced symbol into its sole importer, then delete)."
    )]
    async fn stale_zombie_detector(
        &self,
        Parameters(params): Parameters<StaleZombieParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "stale_zombie_detector",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_stale_zombie::tool_stale_zombie(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "For each god file (line_count >= 500 by default), propose an explicit split \
along FCM topic boundaries with line ranges and a typed `recommended_fix(action=split_file)`. \
USE WHEN: an `architecture_violations` god_module finding or a `design_smell_detection` god_class \
finding has surfaced — this tool turns the diagnosis into a concrete sub-file proposal with chunk \
ranges and per-piece suggested filenames. \
DO NOT USE WHEN: no FCM topics have been computed yet (run `discover_topics` first; otherwise \
this tool soft-fails with `health.topics_present:false`). \
Single-topic god files get an `add_test` recommendation instead — they're cohesive and shouldn't \
be split."
    )]
    async fn recommend_module_split(
        &self,
        Parameters(params): Parameters<RecommendModuleSplitParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "recommend_module_split",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_recommend_module_split::tool_recommend_module_split(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "For each Tarjan SCC (cycle group) in a project's import graph, recommend a \
specific edge to break and the strategy: `extract_interface` or `invert_dependency`. \
USE WHEN: `circular_dependencies` has surfaced cycles and you want explicit, agent-executable fix \
guidance — which edge to flip, which side gets the new abstraction, which imports must update. \
DO NOT USE WHEN: the import graph is empty (graph cron hasn't run); soft-fails with \
`health.graph_stale:true`. \
Strategy heuristic: when one cycle endpoint is more abstract / stable, the edge from the \
less-abstract side becomes a trait/interface dependency on the abstract side (`invert_dependency`); \
otherwise, propose extracting a new shared interface for the lower-coupling endpoint."
    )]
    async fn fix_circular_dependency(
        &self,
        Parameters(params): Parameters<FixCircularDependencyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "fix_circular_dependency",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_fix_circular_dependency::tool_fix_circular_dependency(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "For each shotgun-surgery smell hub (file co-changing with many partners), \
pick the absorbing centroid file and recommend consolidation. \
USE WHEN: a `design_smell_detection` shotgun_surgery finding has surfaced — turn the \"this hub \
ripples to N partners\" signal into a typed `recommended_fix(action=consolidate_logic)` with the \
target file and per-partner moves enumerated. \
DO NOT USE WHEN: git history is disabled for the project (no co-change data); soft-fails with \
`health.git_history_present:false`. \
Centroid heuristic: among the hub plus its co-change partners, pick the file with the highest \
PageRank — the most architecturally central place to consolidate the scattered logic."
    )]
    async fn shotgun_surgery_fix(
        &self,
        Parameters(params): Parameters<ShotgunSurgeryFixParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "shotgun_surgery_fix",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_shotgun_surgery_fix::tool_shotgun_surgery_fix(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Infer a layered architecture for a project (Louvain on imports + SDP-based \
layer assignment) and list every cross-layer import as a violation, each with a typed \
`recommended_fix`. \
USE WHEN: doing an architecture audit and you want a layered view *plus* the violations that \
break it — UI files reaching directly into data layer, deep upward dependencies, etc. \
DO NOT USE WHEN: the project's import graph is small (< num_layers communities) — the heuristic \
collapses and confidence drops sharply. The default web-biased layer-naming is unreliable for \
non-web codebases; override via `layer_names`. \
Per-violation fix dispatch: skip-N-layer downward → add_anti_corruption_layer; small leaf → \
move_function; upward → invert_dependency."
    )]
    async fn recommend_layering(
        &self,
        Parameters(params): Parameters<RecommendLayeringParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "recommend_layering",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_recommend_layering::tool_recommend_layering(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Given a starter file, recommend the right PR scope: minimum (direct \
importers), recommended (+ co-change Jaccard ≥ threshold), maximum (+ depth-N reverse BFS + \
topic neighbors). Emits a `verdict`: focused / normal / sprawling. \
USE WHEN: about to open a PR and want to know whether other files should travel with it. \
DO NOT USE WHEN: git history is disabled — co-change leg drops out and the recommendation \
quality declines (still works on imports + topics)."
    )]
    async fn pr_scope_recommender(
        &self,
        Parameters(params): Parameters<PrScopeRecommenderParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "pr_scope_recommender",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_pr_scope::tool_pr_scope(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Files in the intersection of top-P% PageRank, top-P% churn, and top-P% \
fix_commit_ratio — the most fragile critical paths. \
USE WHEN: deciding where to invest test/docs effort, or as a release-readiness audit (\"what's \
the most expensive risk we're shipping?\"). \
DO NOT USE WHEN: file_metrics or git history is empty — the percentile gates collapse to zero \
and the result is empty. Each row carries a `priority` (P0/P1/P2) and an action recommendation \
(add integration test, freeze API, refactor)."
    )]
    async fn hot_path_audit(
        &self,
        Parameters(params): Parameters<HotPathAuditParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "hot_path_audit",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_hot_path_audit::tool_hot_path_audit(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Per-file knowledge-concentration risk: top author's share of blamed lines × \
PageRank ÷ distinct authors. Surfaces files where a single contributor's departure causes \
maximum harm. \
USE WHEN: planning team coverage / PTO, or auditing a release candidate for fragility. \
DO NOT USE WHEN: file_chunks blame columns are empty (project hasn't run the git-blame cron). \
Returns critical / warning / healthy buckets and a `bus_factor_estimate` (greedy set-cover ≥50% of total blamed lines)."
    )]
    async fn bus_factor_map(
        &self,
        Parameters(params): Parameters<BusFactorMapParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "bus_factor_map",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_bus_factor_map::tool_bus_factor_map(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Given a list of changed files, rank reviewers by recent ownership and \
suggest a minimum cover-set (≥80% of files with the fewest reviewers). \
USE WHEN: about to open a PR and need to pick reviewers — pastes the file list, gets a \
ranked author list with per-file breakdowns. \
DO NOT USE WHEN: blame columns are empty — files with no blame data appear in `unowned_files`. \
Pass the PR author's email in `exclude_authors` to skip self-review."
    )]
    async fn reviewer_recommender(
        &self,
        Parameters(params): Parameters<ReviewerRecommenderParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "reviewer_recommender",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_reviewer_recommender::tool_reviewer_recommender(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Audit external/unresolved import targets across one or all projects. \
USE WHEN: doing a quarterly dep audit — surface third-party deps, rank by usage centrality + \
staleness, recommend prune / upgrade / consolidate / keep. \
DO NOT USE WHEN: code_graph_edges has no unresolved-target rows."
    )]
    async fn dependency_health(
        &self,
        Parameters(params): Parameters<DependencyHealthParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "dependency_health",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_dependency_health::tool_dependency_health(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Snippet-as-query: embed a code snippet and find the closest implementations \
across all indexed projects, plus a `verdict` (reuse / adapt / new). \
USE WHEN: mid-implementation, you want to know whether anyone in the workspace is already \
solving this. Distinct from `semantic_search` (which targets natural-language queries). \
DO NOT USE WHEN: you have a known seed file — use `find_similar_modules`."
    )]
    async fn pattern_search(
        &self,
        Parameters(params): Parameters<PatternSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "pattern_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_pattern_search::tool_pattern_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Score files by likelihood of conflicting with peer in-flight work, using \
overlapping recent commits as a proxy. \
USE WHEN: about to land a long-lived feature branch and want to know which files are also \
being edited concurrently. \
DO NOT USE WHEN: git history is disabled — soft-fails with `health.git_history_present:false`."
    )]
    async fn merge_conflict_risk(
        &self,
        Parameters(params): Parameters<MergeConflictRiskParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "merge_conflict_risk",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_merge_conflict_risk::tool_merge_conflict_risk(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Surface symbols whose naming convention diverges from the dominant \
convention within their directory. \
USE WHEN: enforcing or auditing per-module naming consistency. \
DO NOT USE WHEN: file_symbols data is absent — this tool requires the Tier-0e tree-sitter pass. \
Today, soft-fails with `health.symbols_present:false` and a guidance message; once Phase 0b \
ships, returns `divergences[]` with `recommended_fix(action=move_function)`."
    )]
    async fn naming_consistency(
        &self,
        Parameters(params): Parameters<NamingConsistencyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "naming_consistency",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_naming_consistency::tool_naming_consistency(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Project- or file-level growth trajectory over time: commits, authors, and \
optionally LOC per bucket (week/month/quarter). \
USE WHEN: investigating whether a module is growing fast enough to need a preemptive split, or \
auditing release-velocity trends. \
DO NOT USE WHEN: git history is disabled or the lookback window has < 4 buckets — trend math \
falls back to raw bucket data with no projection."
    )]
    async fn module_growth_trajectory(
        &self,
        Parameters(params): Parameters<ModuleGrowthParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "module_growth_trajectory",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_module_growth::tool_module_growth(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Given a 'modern' reference file, find legacy/older usages of similar \
patterns across the corpus. \
USE WHEN: you've just rewritten a feature and want to know where the old version is still in \
use, so you can plan migrations. \
DO NOT USE WHEN: no chunks found for the reference."
    )]
    async fn adoption_lag(
        &self,
        Parameters(params): Parameters<AdoptionLagParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "adoption_lag",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_adoption_lag::tool_adoption_lag(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Compose a phased remediation plan: aggregate `recommended_fix` items from \
bug_prediction, technical_debt_analysis, architecture_violations, design_smell_detection, \
stale_zombie_detector, and fix_circular_dependency. Rank by cost-benefit and bin-pack into \
'now' / 'next' / 'later' for the requested time_horizon. \
USE WHEN: planning a remediation sprint — one ranked, time-budgeted list across every quality \
dimension instead of running 6 tools separately."
    )]
    async fn tech_debt_burn_down(
        &self,
        Parameters(params): Parameters<TechDebtBurnDownParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tech_debt_burn_down",
            45,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_tech_debt_burn_down::tool_tech_debt_burn_down(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Semantic search over git commit messages and diffs. \
USE WHEN: investigating when a feature was added, when a bug was fixed, how a piece of \
code evolved, or who last touched a concept ('fix database timeout', 'add authentication'). \
DO NOT USE WHEN: you have an exact commit hash (`git show <hash>` is faster) or you only \
need recent commits in the current cwd (`git log` is faster). \
Requires per-project opt-in via [git] index_history = true in .pgmcp.toml.")]
    async fn search_commits(
        &self,
        Parameters(params): Parameters<SearchCommitsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "search_commits",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_search_commits::tool_search_commits(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Semantic search over the dedicated software pattern and anti-pattern knowledge index. \
USE WHEN: designing a feature/refactor and you want pattern candidates, anti-pattern warnings, or paradigm-specific design guidance. \
DO NOT USE WHEN: searching indexed source files — use semantic_search/hybrid_search for code. \
The pattern index is separate from file_chunks and includes locally imported full-text pattern documentation plus curated cards."
    )]
    async fn software_pattern_search(
        &self,
        Parameters(params): Parameters<SoftwarePatternSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "software_pattern_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_software_patterns::tool_software_pattern_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Recommend software design patterns and anti-patterns to avoid for a feature or refactor task. \
USE WHEN: drafting an implementation plan and selecting an approach for a target paradigm. \
Returns structured recommendations with source citations from the separate pattern knowledge index."
    )]
    async fn recommend_design_patterns(
        &self,
        Parameters(params): Parameters<RecommendDesignPatternsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "recommend_design_patterns",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_software_patterns::tool_recommend_design_patterns(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Review a proposed design against the software pattern knowledge index. \
USE WHEN: checking a plan for anti-pattern risks and better paradigm-specific alternatives before implementation."
    )]
    async fn review_design_patterns(
        &self,
        Parameters(params): Parameters<ReviewDesignPatternsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "review_design_patterns",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_software_patterns::tool_review_design_patterns(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Fetch a full software pattern or anti-pattern card by slug or id, with source links and optional excerpts."
    )]
    async fn get_software_pattern(
        &self,
        Parameters(params): Parameters<GetSoftwarePatternParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "get_software_pattern",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_software_patterns::tool_get_software_pattern(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "List software patterns and anti-patterns by paradigm, kind, category, or source family."
    )]
    async fn list_software_patterns(
        &self,
        Parameters(params): Parameters<ListSoftwarePatternsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "list_software_patterns",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_software_patterns::tool_list_software_patterns(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Pattern catalog statistics: paradigms, patterns, source families, chunks, and embedding status."
    )]
    async fn pattern_catalog_stats(
        &self,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "pattern_catalog_stats",
            30,
            &_ctx,
            "",
            super::tools::tool_software_patterns::tool_pattern_catalog_stats(self.ctx()),
        )
        .await
    }

    #[tool(
        description = "Admin tool to seed, import, or re-embed the local full-text software pattern catalog. \
mode=seed_only embeds bundled cards; mode=source_family imports one source family; mode=all imports all registered source URLs."
    )]
    async fn refresh_pattern_catalog(
        &self,
        Parameters(params): Parameters<RefreshPatternCatalogParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // mode=all touches ~50 registered source families; each fetches an
        // article body over HTTP and re-embeds 10-30 chunks. A 10-minute
        // ceiling accommodates that without leaving the call open forever.
        // Per-source progress is committed independently, so a timeout still
        // preserves what landed before the deadline.
        instrumented_tool_wrap(
            self.stats(),
            "refresh_pattern_catalog",
            600,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_software_patterns::tool_refresh_pattern_catalog(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Admin tool to attach full-text local documentation or snippets to an existing software pattern and embed them."
    )]
    async fn upsert_pattern_source(
        &self,
        Parameters(params): Parameters<UpsertPatternSourceParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // Single-source manual ingestion: 5 minutes covers very large pasted
        // bodies (entire books, RFCs) and the per-chunk embedding loop.
        instrumented_tool_wrap(
            self.stats(),
            "upsert_pattern_source",
            300,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_software_patterns::tool_upsert_pattern_source(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Discover semantic code patterns via Fuzzy C-Means clustering on chunk \
embeddings (Fuzzy BERTopic + c-TF-IDF labels). \
USE WHEN: you want to understand the dominant patterns/concerns in a project (intra-project \
DRY violations) or shared patterns across projects (cross-project library candidates). \
DO NOT USE WHEN: you already know the concept and want to find specific instances — use \
`semantic_search` instead. \
With `project`: real-time intra-project. Without: cached cross-project results. Returns \
topic clusters with keyword labels, membership scores, and representative chunks/files."
    )]
    async fn discover_topics(
        &self,
        Parameters(params): Parameters<DiscoverTopicsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "discover_topics",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_discover_topics::tool_discover_topics(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Meta-clustering hierarchy over global topic centroids (Phase 9). Returns FCM-based meta-groups where each meta-group's parent_topic_ids point to the global topics it contains. Complementary view to discover_topics — chunk-to-global-topic assignments remain authoritative for cross-document comparability."
    )]
    async fn topic_hierarchy_fcm(
        &self,
        Parameters(params): Parameters<TopicHierarchyFcmParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "topic_hierarchy_fcm",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_topic_hierarchy_fcm::tool_topic_hierarchy_fcm(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Find chunks/files with low topic membership (below threshold). \
USE WHEN: looking for dead code, abandoned utilities, or candidates for deletion. Orphan \
code is content the topic model couldn't fit anywhere with confidence. \
DO NOT USE WHEN: looking for files whose semantic doesn't match their directory — use \
`find_misplaced_code` for that. \
Requires discover_topics first."
    )]
    async fn find_orphans(
        &self,
        Parameters(params): Parameters<FindOrphansParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "find_orphans",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_find_orphans::tool_find_orphans(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Architecture-recovery: files whose semantic topic doesn't match their \
directory context. \
USE WHEN: looking for files in the wrong module, suggesting reorganization, or auditing \
'why is this in this folder?'. \
DO NOT USE WHEN: looking for orphans (no topic) — use `find_orphans`. \
Compares each file's dominant topic vs its directory neighbors' majority. Requires \
discover_topics first."
    )]
    async fn find_misplaced_code(
        &self,
        Parameters(params): Parameters<FindMisplacedCodeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "find_misplaced_code",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_find_misplaced_code::tool_find_misplaced_code(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Find files that frequently change together in git commits (Jaccard \
co-change coupling). \
USE WHEN: planning a refactor and want to know which files will likely need to change \
together, or assessing whether two files belong in the same module. High coupling >0.7 \
suggests strong implicit dependency. \
DO NOT USE WHEN: looking for static dependencies (use `dependency_graph` instead) or \
semantic similarity (use `find_similar_modules`). \
Requires [git] index_history = true."
    )]
    async fn find_coupled_files(
        &self,
        Parameters(params): Parameters<FindCoupledFilesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "find_coupled_files",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_find_coupled_files::tool_find_coupled_files(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Find topics with implementation code but no test coverage. \
USE WHEN: building a test plan, identifying which subsystems have weak tests, or arguing \
for resourcing test work in specific areas. \
DO NOT USE WHEN: you want line-coverage data — pgmcp doesn't run the tests, only \
classifies files as test/impl based on path heuristics. Use a coverage tool (tarpaulin, \
llvm-cov) for true coverage. \
Requires discover_topics first."
    )]
    async fn test_coverage_gaps(
        &self,
        Parameters(params): Parameters<TestCoverageGapsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "test_coverage_gaps",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_test_coverage_gaps::tool_test_coverage_gaps(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Rank files by composite complexity (size + chunk count + topic diversity \
+ coupling). \
USE WHEN: identifying SRP violations, finding files that 'do too much', or prioritizing \
refactor targets by raw size/diversity. \
DO NOT USE WHEN: you want bug-likelihood (use `bug_prediction`) or formal complexity \
metrics (use `design_metrics` for cyclomatic + WMC + maintainability index). \
Sortable by: composite (default), size, chunks, topics, coupling."
    )]
    async fn complexity_hotspots(
        &self,
        Parameters(params): Parameters<ComplexityHotspotsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "complexity_hotspots",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_complexity_hotspots::tool_complexity_hotspots(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Show how discovered topics relate hierarchically using agglomerative clustering on topic centroids. Reveals module boundaries and related topic groups. Groups with low merge distance contain highly related topics that could be combined."
    )]
    async fn topic_hierarchy(
        &self,
        Parameters(params): Parameters<TopicHierarchyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "topic_hierarchy",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_topic_hierarchy::tool_topic_hierarchy(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Find files (default: markdown) covering overlapping topics that should \
be consolidated. \
USE WHEN: cleaning up a docs/ directory with redundant pages, or finding code modules \
that duplicate concerns. \
DO NOT USE WHEN: looking for line-level duplicates — use `find_duplicates`. This is \
topic-level, not text-level. \
Weighted Jaccard on per-file topic distributions, union-find clustered. Set language=\"*\" \
for all languages."
    )]
    async fn suggest_merges(
        &self,
        Parameters(params): Parameters<SuggestMergesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "suggest_merges",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_suggest_merges::tool_suggest_merges(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Find files spanning too many distinct topics and suggest split points. \
USE WHEN: a markdown file or source module has grown sprawling, or you suspect an SRP \
violation that you want broken up cleanly. \
DO NOT USE WHEN: looking for general complexity hotspots — use `complexity_hotspots`. \
Splits align to heading boundaries (markdown) or chunk boundaries (code). Shannon-entropy \
scored. Requires discover_topics first."
    )]
    async fn suggest_splits(
        &self,
        Parameters(params): Parameters<SuggestSplitsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "suggest_splits",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_suggest_splits::tool_suggest_splits(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Find code topics with no corresponding markdown documentation. \
USE WHEN: building a docs-debt list, finding sub-systems that exist only in code, or \
prioritizing where to write documentation. \
DO NOT USE WHEN: you want to assess docstring quality (comments inside code) — this only \
considers separate markdown files. \
Requires discover_topics first."
    )]
    async fn doc_coverage_gaps(
        &self,
        Parameters(params): Parameters<DocCoverageGapsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "doc_coverage_gaps",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_doc_coverage_gaps::tool_doc_coverage_gaps(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // Phase 2: Graph Analysis tools
    // ========================================================================

    #[tool(
        description = "Project dependency graph: import relationships, optionally focused on a \
file's neighborhood. \
USE WHEN: you need to know what depends on a file, what a file depends on, or want a \
Graphviz diagram of an architecture. \
DO NOT USE WHEN: you need co-change behavior (use `find_coupled_files`) or static call \
graphs (this is import-level only). \
Output formats: summary (counts), edges (list), DOT (Graphviz). Requires graph-analysis cron."
    )]
    async fn dependency_graph(
        &self,
        Parameters(params): Parameters<DependencyGraphParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "dependency_graph",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_dependency_graph::tool_dependency_graph(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Rank files by graph centrality (PageRank, betweenness, degree). \
USE WHEN: identifying load-bearing files in an unfamiliar codebase ('what should I read \
first?'), or finding which files a refactor would impact most. High-centrality = touches \
many other files. \
DO NOT USE WHEN: you want change-frequency or bug-proneness — use `bug_prediction` or \
`complexity_hotspots`. \
Requires graph-analysis cron. The composite `orient` tool returns the top entry points by \
PageRank as part of its envelope."
    )]
    async fn centrality_analysis(
        &self,
        Parameters(params): Parameters<CentralityAnalysisParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "centrality_analysis",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_centrality_analysis::tool_centrality_analysis(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Detect module communities in the dependency graph using Louvain algorithm. Compares discovered communities against directory structure to reveal architectural misalignment. Requires the graph-analysis cron job to have run."
    )]
    async fn community_detection(
        &self,
        Parameters(params): Parameters<CommunityDetectionParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "community_detection",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_community_detection::tool_community_detection(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Find circular import dependency cycles (Tarjan SCC + DFS). \
USE WHEN: investigating build/link errors, code that's hard to test in isolation, or \
auditing layering violations. Cycles make code harder to test, build, and understand. \
DO NOT USE WHEN: looking for runtime call cycles (this is import-level static graph only). \
Requires graph-analysis cron."
    )]
    async fn circular_dependencies(
        &self,
        Parameters(params): Parameters<CircularDependenciesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "circular_dependencies",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_circular_dependencies::tool_circular_dependencies(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Predict which files would be affected by changing a specific file. \
USE WHEN: scoping a refactor or assessing the blast radius of a change before making it. \
Combines reverse-imports + git co-change + semantic similarity for richer impact than any \
single signal. \
DO NOT USE WHEN: you only need static reverse-imports (use `dependency_graph` with focus). \
Requires graph-analysis cron + git history for full coverage."
    )]
    async fn change_impact_analysis(
        &self,
        Parameters(params): Parameters<ChangeImpactAnalysisParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "change_impact_analysis",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_change_impact_analysis::tool_change_impact_analysis(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    // ========================================================================
    // Phase 3: Architecture & Design Quality tools
    // ========================================================================

    #[tool(
        description = "Robert C. Martin package metrics per module: Ca, Ce, Instability (I), \
Abstractness (A), Distance from Main Sequence (D*). \
USE WHEN: doing a formal architecture review, identifying Zone of Pain (low A, low I) or \
Zone of Uselessness (high A, high I) modules. \
DO NOT USE WHEN: looking at single-file complexity — use `design_metrics`. This is \
module/package level. \
Requires graph-analysis cron."
    )]
    async fn coupling_cohesion_report(
        &self,
        Parameters(params): Parameters<CouplingCohesionReportParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "coupling_cohesion_report",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_coupling_cohesion_report::tool_coupling_cohesion_report(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Detect architecture violations: cycles, god modules, bidirectional deps, \
SDP violations, Zone of Pain/Uselessness modules. \
USE WHEN: producing an architecture review, gating a PR on architectural-debt regressions, \
or building an ORR (Operational Readiness Review). \
DO NOT USE WHEN: looking at design-level smells in a single file — use \
`design_smell_detection` for god class / SRP violations / shotgun surgery / etc. \
Grouped by severity. Requires graph-analysis cron."
    )]
    async fn architecture_violations(
        &self,
        Parameters(params): Parameters<ArchitectureViolationsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "architecture_violations",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_architecture_violations::tool_architecture_violations(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "File-level design smells: god class, SRP violation, shotgun surgery, \
stale module, unstable dependency. \
USE WHEN: doing a code review for design quality, finding refactor targets at the file \
level. Each smell has a clear remediation pattern. \
DO NOT USE WHEN: looking for module/package-level violations — use `architecture_violations` \
for those. \
Filter to specific smell types via `smells` param. Requires graph-analysis + discover_topics."
    )]
    async fn design_smell_detection(
        &self,
        Parameters(params): Parameters<DesignSmellDetectionParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "design_smell_detection",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_design_smell_detection::tool_design_smell_detection(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "10-dimension architecture-quality scorecard (separation of concerns, \
loose coupling, SDP compliance, acyclicity, test coverage, doc coverage, code organization, \
module balance, API stability, dependency health). \
USE WHEN: producing an architecture review or maturity assessment, comparing two projects \
on aggregate quality. \
DO NOT USE WHEN: you want the full A-F engineering scorecard with ORR checklist — use \
`engineering_scorecard` (this tool is one of its inputs). \
Each dim 0-100%. Requires graph-analysis + discover_topics."
    )]
    async fn architecture_quality(
        &self,
        Parameters(params): Parameters<ArchitectureQualityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "architecture_quality",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_architecture_quality::tool_architecture_quality(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Per-file design metrics: cyclomatic complexity, WMC, Card & Glass S/D/Sy, \
maintainability index. \
USE WHEN: ranking refactor targets by formal numeric metrics, or comparing complexity \
between two files objectively. \
DO NOT USE WHEN: you want a composite ranking (use `complexity_hotspots`) or bug \
prediction (use `bug_prediction`). \
Pure metrics, no interpretation. Useful in scorecards and CI gates."
    )]
    async fn design_metrics(
        &self,
        Parameters(params): Parameters<DesignMetricsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "design_metrics",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_design_metrics::tool_design_metrics(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // Phase 4: ML Prediction tools (heuristic-based, no ML dependencies)
    // ========================================================================

    #[tool(
        description = "Heuristic bug-proneness ranking per file (churn × complexity × fix-commit \
ratio × coupling). \
USE WHEN: prioritizing review/test-coverage effort, or identifying risky files to refactor \
first. \
DO NOT USE WHEN: looking at a single file (use `complexity_hotspots` and \
`technical_debt_analysis` for richer per-file detail). \
Heuristic, not ML. Requires graph-analysis cron + git history."
    )]
    async fn bug_prediction(
        &self,
        Parameters(params): Parameters<BugPredictionParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "bug_prediction",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_bug_prediction::tool_bug_prediction(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Composite technical-debt score per file (TODO density + cyclomatic \
complexity + test gaps + D* + churn). \
USE WHEN: building a refactor backlog, identifying highest-leverage cleanup targets, or \
estimating debt for an architecture review. \
DO NOT USE WHEN: looking at a specific file's complexity in isolation — `design_metrics` \
gives per-file numbers without the composite weighting. \
Optionally scans content for TODO/FIXME/HACK markers."
    )]
    async fn technical_debt_analysis(
        &self,
        Parameters(params): Parameters<TechnicalDebtAnalysisParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "technical_debt_analysis",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_technical_debt_analysis::tool_technical_debt_analysis(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Statistical outlier detection: files whose embedding distance from \
project centroid + metric z-scores deviate from the project norm. \
USE WHEN: hunting for abandoned experiments, copy-pasted code from other projects, or \
architectural inconsistencies the model can't see by reading any single file. \
DO NOT USE WHEN: looking for misplaced files relative to directory context — use \
`find_misplaced_code` (semantic-based, more targeted). \
No ML deps — pure statistical distance."
    )]
    async fn anomaly_detection(
        &self,
        Parameters(params): Parameters<AnomalyDetectionParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "anomaly_detection",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_anomaly_detection::tool_anomaly_detection(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Tornhill-style hotspot intersection: functions where high churn meets high \
complexity (Adam Tornhill, *Your Code as a Crime Scene*). \
USE WHEN: prioritizing refactoring — combines bug-proneness signals (churn) with \
maintenance-cost signals (cyclomatic, cognitive, low MI) at function granularity. \
Returns per-function rows with score, file, language, churn rate, commit count, \
cyclomatic, cognitive, MI, NPath. \
Modes: \"intersect\" (default, churn AND complexity), \"union\" (OR), \"max\" (rank by composite, no filter). \
Requires both `file_metrics` (graph-analysis cron) and `function_metrics` (function-metrics cron) populated."
    )]
    async fn code_on_fire(
        &self,
        Parameters(params): Parameters<CodeOnFireParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "code_on_fire",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_code_on_fire::tool_code_on_fire(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Unified documented-tech-debt surface across the project: comment markers \
(TODO/FIXME/HACK/XXX/TEMP/WORKAROUND/NOTE/TBD/REVIEW/KLUDGE/BUG/OPTIMIZE/DEPRECATED/SMELL/REFACTOR/WTF/DEBUG), \
stub macros (Rust todo!()/unimplemented!()/unreachable!()/panic!(\"not implemented\") + Python raise NotImplementedError + \
JS/TS throw new Error(\"not implemented\") + Go panic(\"TODO\") + Java UnsupportedOperationException + C/C++ __builtin_unreachable), \
and deprecation annotations (#[deprecated] / @Deprecated / @deprecated / DeprecationWarning). \
Returns per-kind counts, severity tiers (high/medium/low), GitHub-issue refs (#1234, owner/repo#42), and git-blame attribution \
(author + age_days). Modes: \"summary\" (counts only, default), \"full\" (per-occurrence list)."
    )]
    async fn documented_tech_debt(
        &self,
        Parameters(params): Parameters<DocumentedTechDebtParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "documented_tech_debt",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_documented_tech_debt::tool_documented_tech_debt(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Trigger a heavy maintenance cron on demand: symbol-extraction (populates file_symbols + symbol_references), \
call-graph (populates symbol_references call edges), or function-metrics (cyclomatic/cognitive/Halstead/NPath/MI). \
USE WHEN: dead_code_reachability or naming_consistency returns health.symbols_present:false because the cron hasn't run \
yet. The same daemon's normal 30-min-after-Ready / 2-h-interval schedule still applies; this just lets the operator skip the wait. \
Each invocation runs to completion (no background queuing); typical durations are 30-120s on a workspace with ~10k files."
    )]
    async fn trigger_cron(
        &self,
        Parameters(params): Parameters<TriggerCronParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // Use a long inner timeout (5 min) since these crons can run
        // longer than the default 30 s tool budget on large workspaces.
        instrumented_tool_wrap(
            self.stats(),
            "trigger_cron",
            300,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_trigger_cron::tool_trigger_cron(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // A2A inter-agent IPC bridge — outbound MCP-side tools
    // ========================================================================
    #[tool(
        description = "Dispatch a Task to a registered A2A peer agent. Returns the final Task with status and artifacts."
    )]
    async fn a2a_send_task(
        &self,
        Parameters(params): Parameters<A2aSendTaskParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_send_task",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_a2a_send_task::tool_a2a_send_task(self.ctx(), params),
        )
        .await
    }
    #[tool(description = "Poll a Task on a registered A2A peer agent.")]
    async fn a2a_get_task(
        &self,
        Parameters(params): Parameters<A2aGetTaskParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_get_task",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_a2a_get_task::tool_a2a_get_task(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Return the SSE URL for streaming events from a peer's Task. Caller opens the URL with Accept: text/event-stream."
    )]
    async fn a2a_subscribe_task(
        &self,
        Parameters(params): Parameters<A2aSubscribeTaskParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_subscribe_task",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_a2a_subscribe_task::tool_a2a_subscribe_task(self.ctx(), params),
        )
        .await
    }
    #[tool(description = "Cancel a Task on a registered A2A peer agent.")]
    async fn a2a_cancel_task(
        &self,
        Parameters(params): Parameters<A2aCancelTaskParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_cancel_task",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_a2a_cancel_task::tool_a2a_cancel_task(self.ctx(), params),
        )
        .await
    }
    #[tool(description = "Register a peer A2A agent in the local directory. Upserts by name.")]
    async fn a2a_register_agent(
        &self,
        Parameters(params): Parameters<A2aRegisterAgentParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_register_agent",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_a2a_register_agent::tool_a2a_register_agent(self.ctx(), params),
        )
        .await
    }
    #[tool(description = "List all registered A2A peer agents in the local directory.")]
    async fn a2a_list_agents(
        &self,
        Parameters(params): Parameters<A2aListAgentsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_list_agents",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_a2a_list_agents::tool_a2a_list_agents(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // A2A RecursiveMAS-inspired extensions (Yang et al. 2026 Table 1)
    // ========================================================================

    #[tool(
        description = "Find registered A2A peers matching specialty tags / role. \
Useful before invoking a collaboration pattern so you can pick the right peer for each role."
    )]
    async fn a2a_find_agents_by_specialty(
        &self,
        Parameters(params): Parameters<A2aFindAgentsBySpecialtyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_find_agents_by_specialty",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_a2a_find_agents_by_specialty::tool_a2a_find_agents_by_specialty(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Sequential collaboration pattern: Planner → Critic → Solver. \
Threads three peer agents in order; each round's output conditions the next. \
RecursiveMAS Table 1 Sequential Style."
    )]
    async fn a2a_pattern_sequential(
        &self,
        Parameters(params): Parameters<A2aPatternSequentialParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_pattern_sequential",
            300,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_a2a_pattern_sequential::tool_a2a_pattern_sequential(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Mixture collaboration pattern: fan out to N specialist peers in parallel + Summarizer aggregation. \
RecursiveMAS Table 1 Mixture Style."
    )]
    async fn a2a_pattern_mixture(
        &self,
        Parameters(params): Parameters<A2aPatternMixtureParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_pattern_mixture",
            300,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_a2a_pattern_mixture::tool_a2a_pattern_mixture(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Distillation collaboration pattern: Expert → Learner pair with latency / compression comparison. \
RecursiveMAS Table 1 Distillation Style."
    )]
    async fn a2a_pattern_distillation(
        &self,
        Parameters(params): Parameters<A2aPatternDistillationParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_pattern_distillation",
            300,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_a2a_pattern_distillation::tool_a2a_pattern_distillation(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Deliberation collaboration pattern: Reflector ↔ Tool-Caller iterative loop until convergence. \
RecursiveMAS Table 1 Deliberation Style."
    )]
    async fn a2a_pattern_deliberation(
        &self,
        Parameters(params): Parameters<A2aPatternDeliberationParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_pattern_deliberation",
            300,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_a2a_pattern_deliberation::tool_a2a_pattern_deliberation(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "List the CSM/MPST coordination protocols (the five RecursiveMAS patterns) \
with participants and well-formedness. ADR-009."
    )]
    async fn csm_list_protocols(
        &self,
        Parameters(params): Parameters<CsmListProtocolsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "csm_list_protocols",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_csm_list_protocols::tool_csm_list_protocols(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Show one coordination pattern's global type (the MPST AST), participants, \
and well-formedness. ADR-009."
    )]
    async fn csm_protocol_of_pattern(
        &self,
        Parameters(params): Parameters<CsmProtocolOfPatternParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "csm_protocol_of_pattern",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_csm_protocol_of_pattern::tool_csm_protocol_of_pattern(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Show the per-role local machines a coordination pattern projects to \
(G ↾ role); a role that does not project surfaces its projection error. ADR-009."
    )]
    async fn csm_show_projection(
        &self,
        Parameters(params): Parameters<CsmShowProjectionParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "csm_show_projection",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_csm_show_projection::tool_csm_show_projection(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Validate a completed a2a_pattern_* run against its coordination protocol: \
lift the recorded transcript into a trace, check conformance, and persist the verdict to \
csm_run_traces. ADR-009."
    )]
    async fn csm_validate_run(
        &self,
        Parameters(params): Parameters<CsmValidateRunParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "csm_validate_run",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_csm_validate_run::tool_csm_validate_run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Show the protocol interpreter's prescribed orchestrator communication order \
for a pattern (the ProtocolDriver plan). Linear patterns (sequential/mixture/distillation/recursive) \
are drivable; Deliberation is not (runtime choice). ADR-009 Phase 6."
    )]
    async fn csm_protocol_plan(
        &self,
        Parameters(params): Parameters<CsmProtocolPlanParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "csm_protocol_plan",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_csm_protocol_plan::tool_csm_protocol_plan(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Infer a peer's behaviour FSM from a protocol's accumulated run traces \
(passive prefix-tree automaton with observation counts) and diff it against the declared protocol — \
novel symbols flag off-protocol behaviour. ADR-009 Phase 8."
    )]
    async fn csm_infer_peer_fsm(
        &self,
        Parameters(params): Parameters<CsmInferPeerFsmParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "csm_infer_peer_fsm",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_csm_infer_peer_fsm::tool_csm_infer_peer_fsm(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Report that an approach worked / failed for a kind of task. Records it to the \
shared best-practice memory graph (agent_outcomes + a mirrored observation) so peer agents can learn \
what works and what does not. Part A cross-agent best-practice exchange."
    )]
    async fn a2a_report_outcome(
        &self,
        Parameters(params): Parameters<A2aReportOutcomeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // Attribute to the MCP client (claude-code / codex / …) unless the
        // caller supplied an explicit agent_id.
        let mut params = params;
        if params.agent_id.is_none() {
            params.agent_id = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "a2a_report_outcome",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_a2a_report_outcome::tool_a2a_report_outcome(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Open a scientific experiment and PRE-REGISTER its acceptance criterion (anti-p-hacking), \
then receive the server-prescribed PROTOCOL: required sample size (power analysis), the recommended statistical \
test, warm-up, the data schema to submit, and a reproducibility checklist (CPU pinning, governor, hardware/seed \
capture). USE for optimizations, feature refactors, feature additions, bug fixes, and diagnostic deep-dives. The \
AGENT runs the work; the server dictates the methodology. Returns {experiment_id, hypothesis_id, slug, protocol}."
    )]
    async fn experiment_open(
        &self,
        Parameters(params): Parameters<ExperimentOpenParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "experiment_open",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_experiments::tool_experiment_open(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "(Re)fetch the prescribed protocol for an experiment/hypothesis — e.g. after supplying a \
refined expected effect size to tighten the required sample count. Read-only. Returns the kind-aware protocol."
    )]
    async fn experiment_protocol(
        &self,
        Parameters(params): Parameters<ExperimentProtocolParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "experiment_protocol",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_experiments::tool_experiment_protocol(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Submit RAW per-replicate (or per-unit) samples for one arm/metric of an experiment. The \
server stores them, upserts the run with the reported host_meta (hardware/governor/pinning), and VALIDATES \
conformance against the prescribed protocol (sample count, warm-up). Use unit_keys for paired structural metrics."
    )]
    async fn experiment_record_measurement(
        &self,
        Parameters(params): Parameters<ExperimentRecordMeasurementParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "experiment_record_measurement",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_experiments::tool_experiment_record_measurement(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Run the PRE-REGISTERED statistical test on the recorded samples and render the verdict \
(accepted/rejected/inconclusive). Refuses if the criterion was locked after measurements began. Persists the \
decision, sets the hypothesis verdict, mirrors to the memory graph (PROV), and optionally graduates the result \
into the cross-agent best-practice ledger. Returns {verdict, test_type, statistic, p_value, effect_size, CI}."
    )]
    async fn experiment_decide(
        &self,
        Parameters(params): Parameters<ExperimentDecideParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "experiment_decide",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_experiments::tool_experiment_decide(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "CROSS-PROJECT recall: \"has anyone tried X / what worked for Y / what refactor reduced \
coupling in Z\". Semantic + full-text search over experiments, hypotheses, and decisions across ALL projects \
(omit project_id). Filter by kind/verdict. Returns ranked experiments with verdict, p-value, and effect size."
    )]
    async fn experiment_search(
        &self,
        Parameters(params): Parameters<ExperimentSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "experiment_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_experiments::tool_experiment_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Fetch one experiment's full record: hypotheses (with their frozen criteria and verdicts) \
and all decisions. Use experiment_id or slug."
    )]
    async fn experiment_get(
        &self,
        Parameters(params): Parameters<ExperimentGetParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "experiment_get",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_experiments::tool_experiment_get(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "List experiments (paged), filterable by project / kind / status, newest first."
    )]
    async fn experiment_list(
        &self,
        Parameters(params): Parameters<ExperimentListParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "experiment_list",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_experiments::tool_experiment_list(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "The ordered event stream for an experiment (open → criterion locks → runs → decisions) — \
the narrative of how it unfolded, useful for rendering a ledger or reviewing a diagnostic hypothesis chain."
    )]
    async fn experiment_timeline(
        &self,
        Parameters(params): Parameters<ExperimentTimelineParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "experiment_timeline",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_experiments::tool_experiment_timeline(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Capture an ad-hoc profiling/benchmark/debug artifact (perf report, hyperfine/criterion \
JSON, massif, flamegraph, log) — tied to an experiment or free-standing. With parse=true, hyperfine/criterion \
JSON is summarized into metrics. Indexed + embedded so `experiment_search`/grep can later find it."
    )]
    async fn experiment_log_artifact(
        &self,
        Parameters(params): Parameters<ExperimentLogArtifactParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "experiment_log_artifact",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_experiments::tool_experiment_log_artifact(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Render an experiment's structured record to a committed markdown ledger under \
docs/scientific-ledger/ (with YAML frontmatter carrying the slug join-key). dry_run=true returns the markdown \
without writing. The structured record is the source of truth; the ledger is the human-readable, indexed view."
    )]
    async fn experiment_render_ledger(
        &self,
        Parameters(params): Parameters<ExperimentRenderLedgerParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "experiment_render_ledger",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_experiments::tool_experiment_render_ledger(self.ctx(), params),
        )
        .await
    }

    // ── Work-item / plan tracker subsystem ──────────────────────────────────

    #[tool(
        description = "Create a work item (plan/goal/epic/task/sub_task/todo/fixme/idea/note/question/\
nice_to_have/action_item/experiment), optionally under a parent and scoped to a project. USE WHEN you need to \
record a tracked unit of work or decompose a plan into a hierarchy. DO NOT USE WHEN you just want a free-form \
note to yourself outside the tracker. Returns the created row (with its generated public_id)."
    )]
    async fn work_item_create(
        &self,
        Parameters(params): Parameters<WorkItemCreateParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_create",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_create(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Fetch one work item by its public_id, optionally with its full descendant subtree. USE \
WHEN you need the current state of a specific item (status, priority, parent, timestamps). DO NOT USE WHEN you \
want to browse/filter many items — use work_item_list instead. Returns {item, subtree?}."
    )]
    async fn work_item_get(
        &self,
        Parameters(params): Parameters<WorkItemGetParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_get",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_get(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Update a work item's mutable non-status fields (title, body, priority, weight) by \
public_id; omitted fields are left unchanged. USE WHEN re-grooming an item. DO NOT USE WHEN you want to change \
its lifecycle status — use work_item_set_status (status transitions are gated). Returns the updated row."
    )]
    async fn work_item_update(
        &self,
        Parameters(params): Parameters<WorkItemUpdateParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_update",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_update(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "List work items (newest/highest-priority first), filterable by project, kind, status, and \
parent public_id. USE WHEN browsing or triaging the backlog. DO NOT USE WHEN you already know the exact \
public_id — use work_item_get. Returns an array of rows."
    )]
    async fn work_item_list(
        &self,
        Parameters(params): Parameters<WorkItemListParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_list",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_list(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Return a work item and its entire descendant subtree (depth-ordered) by public_id. USE \
WHEN you need the materialized hierarchy under a plan/epic for roll-up or rendering. DO NOT USE WHEN you only \
need the single item — use work_item_get. Returns an array of rows ordered by depth then priority."
    )]
    async fn work_item_tree(
        &self,
        Parameters(params): Parameters<WorkItemTreeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_tree",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_tree(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Move a work item (and its subtree) under a new parent by public_id, or to the root \
(omit new_parent_public_id). USE WHEN re-organizing the hierarchy. DO NOT USE WHEN the target parent is the item \
itself or one of its own descendants — that is rejected to prevent a cycle. Returns the updated row."
    )]
    async fn work_item_reparent(
        &self,
        Parameters(params): Parameters<WorkItemReparentParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_reparent",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_reparent(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Transition a work item's lifecycle status by public_id, AS THE AGENT. USE WHEN advancing \
your own work (ready, in_progress, blocked, claimed_done, verifying, cancelled). DO NOT USE to mark work \
verified/deferred/rejected — the agent actor cannot reach those states (they require user negotiation or \
gatekeeper evidence); such a request is refused with an explanatory error. Returns the updated row."
    )]
    async fn work_item_set_status(
        &self,
        Parameters(params): Parameters<WorkItemSetStatusParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_set_status",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_set_status(self.ctx(), params),
        )
        .await
    }

    // ── Phase 2: tags + progress ────────────────────────────────────────────

    #[tool(
        description = "Create (or upsert) a shared tag in the catalog, addressed by a stable slug derived \
from the name. USE WHEN you want a reusable label to attach across many work items (e.g. 'urgent', 'tech-debt'). \
DO NOT USE WHEN you just want to attach an existing tag to one item — use work_item_tag. Re-running with the \
same name updates the color/description without clobbering existing values. Returns the tag row."
    )]
    async fn tag_create(
        &self,
        Parameters(params): Parameters<TagCreateParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tag_create",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_tag_create(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "List tags in the catalog, ordered by name. USE WHEN browsing the available labels or \
building a tag picker. DO NOT USE WHEN you want the tags ON a specific item — fetch the item (work_item_tag \
returns its current tags). By default returns active tags only; pass include_merged=true to also see \
tombstoned (merged) tags. Returns an array of tag rows."
    )]
    async fn tag_list(
        &self,
        Parameters(params): Parameters<TagListParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tag_list",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_tag_list(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Merge one tag into another: repoint every item tagged with src so it is tagged with \
dst instead, then tombstone src (its slug still resolves to dst). USE WHEN consolidating duplicate/synonym \
tags. DO NOT USE WHEN you merely want to rename a tag — use tag_rename (which keeps the slug stable). src/dst \
may be slugs or labels. Returns {merged: <count>, into: <dst_slug>}; an unknown tag is an invalid_params error."
    )]
    async fn tag_merge(
        &self,
        Parameters(params): Parameters<TagMergeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tag_merge",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_tag_merge(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Rename a tag in place by slug; the slug is intentionally preserved so existing \
references survive. USE WHEN fixing a label's display name. DO NOT USE WHEN you want to fold two tags together \
— use tag_merge. The lookup key is slugified, so you may pass either the slug or the original label. Returns \
the updated tag row; a missing tag is an invalid_params error."
    )]
    async fn tag_rename(
        &self,
        Parameters(params): Parameters<TagRenameParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tag_rename",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_tag_rename(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Attach one or more tags to a work item (by public_id), auto-creating unknown tags by \
default. USE WHEN labeling an item for triage/filtering. DO NOT USE WHEN you want to define a tag's \
metadata (color/description) — use tag_create. With auto_create=false, unknown tags are returned under \
'skipped' instead of being created. Returns {item, applied:[slugs], skipped:[names], tags:[current tags]}."
    )]
    async fn work_item_tag(
        &self,
        Parameters(params): Parameters<WorkItemTagParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_tag",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_tag(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Detach a single tag from a work item (by public_id). USE WHEN a label no longer \
applies. DO NOT USE WHEN you want to delete the tag globally — untag only removes the item↔tag pairing, the \
catalog tag remains. The tag is slugified for lookup; an unknown tag is an invalid_params error. Returns \
{removed: <bool>} (false if the pairing did not exist)."
    )]
    async fn work_item_untag(
        &self,
        Parameters(params): Parameters<WorkItemUntagParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_untag",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_untag(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Append a progress note to a work item (by public_id), optionally with a self-reported \
percent that updates the item's claimed_percent. USE WHEN recording incremental progress / an activity-feed \
entry as you work. DO NOT USE to change the item's lifecycle status — use work_item_set_status. The note is \
recorded as provenance='agent_write' (the agent's claim, NOT trusted for the verified roll-up). Returns the \
new progress row."
    )]
    async fn work_item_record_progress(
        &self,
        Parameters(mut params): Parameters<WorkItemRecordProgressParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.agent_id.is_none() {
            params.agent_id = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "work_item_record_progress",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_record_progress(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Read a work item's progress log, newest first (by public_id). USE WHEN reviewing the \
activity history / how an item progressed over time. DO NOT USE WHEN you only need the current status or \
claimed_percent — use work_item_get. Returns an array of progress rows (note, percent, provenance, timestamps)."
    )]
    async fn work_item_progress_log(
        &self,
        Parameters(params): Parameters<WorkItemProgressLogParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_progress_log",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_progress_log(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Weighted completion roll-up of a work item's subtree. USE WHEN you need overall \
progress of a plan/epic/goal. Returns BOTH verified_* (trustworthy: only evidence-verified leaves count) and \
claimed_* (advisory: also counts agent-reported claimed_done). DO NOT treat claimed_* as actually done."
    )]
    async fn work_item_completion(
        &self,
        Parameters(params): Parameters<WorkItemCompletionParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_completion",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_completion(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Recompute computed_score for active items (recency × manual priority × \
dependency-unblock) and return a now/next/later work plan of the top items. USE WHEN deciding what to work \
on next across a backlog. DO NOT USE WHEN you just want a filtered list — use work_item_list."
    )]
    async fn work_item_reprioritize(
        &self,
        Parameters(params): Parameters<WorkItemReprioritizeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_reprioritize",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_reprioritize(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Semantic search over the work-item backlog by meaning (cosine over BGE-M3 \
embeddings). USE WHEN finding items related to a concept/topic across the tracker. DO NOT USE WHEN you have \
an exact public_id (use work_item_get) or want a structured filter (use work_item_list)."
    )]
    async fn work_item_search(
        &self,
        Parameters(params): Parameters<WorkItemSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Define a reusable plan template + its dictated structural rules (required kinds, \
allowed/required child kinds, min/max children, required fields, required acceptance criteria, \
quantifier-needs-corpus, naming/id regex, max-depth advice). Plan instances are checked against it with \
plan_validate. Re-defining a (slug, version) replaces its rule set."
    )]
    async fn plan_define(
        &self,
        Parameters(params): Parameters<PlanDefineParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "plan_define",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_plan_define(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Validate a plan instance (the subtree under root_public_id) against a plan definition's \
rules; returns a severity-sorted violations report (advisory — reports, does not block). USE WHEN checking a \
plan conforms to a template. DO NOT confuse with verification — that gates on evidence, not structure."
    )]
    async fn plan_validate(
        &self,
        Parameters(params): Parameters<PlanValidateParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "plan_validate",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_plan_validate(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Export a stored plan definition (metadata + [scope] passthrough + rules) to \
serene-eclipse-shaped TOML. Always returns the TOML string; if 'path' is given, also writes the file. USE \
WHEN producing a portable/inspectable .claude/tasks/<slug>.toml artifact. DB stays the source of truth."
    )]
    async fn plan_definition_export(
        &self,
        Parameters(params): Parameters<PlanDefinitionExportParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "plan_definition_export",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_plan_definition_export(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Import a serene-eclipse-shaped TOML definition ([definition] + optional [scope] + \
[[rule]]) into the tracker — inline via 'toml' or from a file via 'path'. Idempotent on (slug, version); \
replaces the rule set and stores the raw TOML in body_toml. USE WHEN loading a shared/edited plan template."
    )]
    async fn plan_definition_import(
        &self,
        Parameters(params): Parameters<PlanDefinitionImportParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "plan_definition_import",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_plan_definition_import(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Attach a machine-checkable acceptance criterion to an item (its definition-of-done). \
USE WHEN specifying what must pass for a task to be verifiable. Pair with record_evidence + attempt_verify."
    )]
    async fn work_item_add_criterion(
        &self,
        Parameters(params): Parameters<WorkItemAddCriterionParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_add_criterion",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_add_criterion(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Record evidence for an acceptance criterion. NOTE: MCP-recorded evidence is \
source='manual' and CANNOT satisfy the verified gate (agents cannot self-verify) — trusted evidence comes \
from CI / the Stop-hook (REST) or the experiment engine."
    )]
    async fn work_item_record_evidence(
        &self,
        Parameters(params): Parameters<WorkItemRecordEvidenceParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_record_evidence",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_record_evidence(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Attempt the gatekeeper →verified transition for an item; succeeds only when every \
required criterion has passing, trusted-source evidence, else returns the explanatory refusal. The item must \
be in claimed_done or verifying."
    )]
    async fn work_item_attempt_verify(
        &self,
        Parameters(params): Parameters<WorkItemAttemptVerifyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_attempt_verify",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_attempt_verify(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "USER-only: defer (explicitly skip) an item so it is excluded from completion \
roll-up. Requires the tracker user_token — an agent CANNOT self-defer (no token; →deferred has no agent arm \
in the transition matrix). Records an append-only scope-negotiation."
    )]
    async fn work_item_defer(
        &self,
        Parameters(params): Parameters<WorkItemDeferParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_defer",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_defer(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "USER-only: reinstate a deferred item (deferred → in_progress). Requires the tracker \
user_token."
    )]
    async fn work_item_reinstate(
        &self,
        Parameters(params): Parameters<WorkItemReinstateParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_reinstate",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_reinstate(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Auto-translate an agent's markdown plan into a tracked work_items subtree \
(headings→plan/epic/task/sub_task, checklists→todos, numbered→sub_tasks, 'acceptance:' lines→criteria). \
Idempotent on re-ingest — preserves status/progress. Optionally validates against a plan definition."
    )]
    async fn work_item_ingest_plan(
        &self,
        Parameters(params): Parameters<WorkItemIngestPlanParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_ingest_plan",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_ingest_plan(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Promote a discovered code marker (TODO/FIXME/HACK/…) into a tracked work item \
(fixme/todo). Idempotent on the marker text+location. USE WHEN turning documented_tech_debt findings into \
trackable items."
    )]
    async fn work_item_promote_marker(
        &self,
        Parameters(params): Parameters<WorkItemPromoteMarkerParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_promote_marker",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_promote_marker(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Atomically claim a work item to work on it (open→in_progress). USE WHEN starting work \
on a shared plan so other agents see it's taken. Returns claimed:false (with the current owner) if another \
agent holds it, it's blocked by a dependency, or it's terminal. Leases auto-expire (crash-safe)."
    )]
    async fn work_item_claim(
        &self,
        Parameters(mut params): Parameters<WorkItemClaimParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.agent_id.is_none() {
            params.agent_id = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "work_item_claim",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_claim(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Claim the next available (unclaimed, unblocked, ready) item, top by priority/score — \
optionally within a plan subtree. The fan-out execution primitive: N agents each get a distinct item. \
Returns claimed:false when the queue is empty."
    )]
    async fn work_item_claim_next(
        &self,
        Parameters(mut params): Parameters<WorkItemClaimNextParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.agent_id.is_none() {
            params.agent_id = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "work_item_claim_next",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_claim_next(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Release your claim on an item (owner-gated). USE WHEN you stop working on a claimed \
item so another agent can pick it up."
    )]
    async fn work_item_release(
        &self,
        Parameters(mut params): Parameters<WorkItemReleaseParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.agent_id.is_none() {
            params.agent_id = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "work_item_release",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_release(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Hand off your claim on an item to another agent (owner-gated re-key). USE WHEN \
delegating a claimed item to a peer agent."
    )]
    async fn work_item_handoff(
        &self,
        Parameters(mut params): Parameters<WorkItemHandoffParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.agent_id.is_none() {
            params.agent_id = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "work_item_handoff",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_handoff(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Heartbeat: mark this agent active and renew the leases on all items it holds (one \
call). USE WHEN working a long-running claimed item so the lease doesn't expire and let another agent steal it."
    )]
    async fn agent_heartbeat(
        &self,
        Parameters(mut params): Parameters<AgentHeartbeatParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.agent_id.is_none() {
            params.agent_id = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "agent_heartbeat",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_agent_heartbeat(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Who currently owns a work item + its claim/handoff history. USE WHEN checking whether \
an item is being worked and by whom before claiming it."
    )]
    async fn work_item_who_owns(
        &self,
        Parameters(params): Parameters<WorkItemWhoOwnsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_who_owns",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_who_owns(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "What an agent is doing (its presence + currently-claimed items + workload), or — with \
no agent_id — the active-agent roster ('who is working'). USE WHEN coordinating multiple agents."
    )]
    async fn agent_activity(
        &self,
        Parameters(params): Parameters<AgentActivityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "agent_activity",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_agent_activity(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "The workspace (or plan-scoped) activity feed: recent progress + claim/handoff events, \
newest first, agent-attributed. USE WHEN reviewing 'what is happening' across the tracker or on a plan."
    )]
    async fn work_item_activity(
        &self,
        Parameters(params): Parameters<WorkItemActivityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_activity",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_activity(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Link two work items with a typed relation (blocks | depends_on | relates_to | \
duplicates | supersedes | derived_from). The ordering relations (depends_on/blocks) are REJECTED if they \
would create a dependency cycle (an unschedulable loop). USE WHEN recording that one item blocks/depends-on \
another, duplicates it, or supersedes it."
    )]
    async fn work_item_link(
        &self,
        Parameters(mut params): Parameters<WorkItemLinkParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.created_by.is_none() {
            params.created_by = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "work_item_link",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_link(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Remove a typed relation between two work items. Returns {removed: bool}. USE WHEN a \
dependency/blocks/duplicates link no longer holds."
    )]
    async fn work_item_unlink(
        &self,
        Parameters(params): Parameters<WorkItemUnlinkParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_unlink",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_unlink(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Report dependency cycles in the schedule graph (depends_on + blocks). Each cycle is a \
strongly-connected component of size > 1; an empty report (is_dag=true) means the schedule is a valid DAG. \
USE WHEN diagnosing why items are stuck or after a bulk import that bypassed the per-edge cycle guard."
    )]
    async fn work_item_cycles(
        &self,
        Parameters(params): Parameters<WorkItemCyclesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_cycles",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_cycles(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Anchor a work item to a code location (a file path and/or an explicit chunk_id/symbol_id; \
at least one must resolve). USE WHEN tying a task/clause to the precise code it concerns — feeds the auditor \
and change-impact surfaces."
    )]
    async fn work_item_anchor_code(
        &self,
        Parameters(params): Parameters<WorkItemAnchorCodeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_anchor_code",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_anchor_code(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Burndown/velocity for a plan: verified-vs-remaining counts, realized velocity \
(items verified/day over the window), and a naive ETA. USE WHEN reporting plan progress or estimating \
completion. Reads the append-only status history — reflects evidence-verified completion, not agent claims."
    )]
    async fn work_item_burndown(
        &self,
        Parameters(params): Parameters<WorkItemBurndownParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_burndown",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_burndown(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Export a plan subtree as a markdown task list or an Org-mode outline (status → \
checkbox/keyword). USE WHEN sharing or archiving a plan as a portable document. Returns the rendered text."
    )]
    async fn work_item_export(
        &self,
        Parameters(params): Parameters<WorkItemExportParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_export",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_export(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Link a scientific experiment to the tracker as a kind='experiment' task (auto-created \
if no work_item_public_id is given) and seed an 'experiment_verdict' criterion. The experiment then gains \
priority/tags/progress/roll-up/claiming, and experiment_decide posts its statistical verdict as trusted \
evidence that auto-verifies the task. USE WHEN you want an experiment tracked + verified like any other task."
    )]
    async fn work_item_link_experiment(
        &self,
        Parameters(params): Parameters<WorkItemLinkExperimentParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_link_experiment",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::work_items::tool_work_item_link_experiment(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Recursive Language Model decomposition (Part B): treat a corpus/file as an external \
environment, decompose into snippets, recursively sub-call a peer LM over each (small context), and stitch \
the partials — the full context is never inlined. Solves beyond-context-window queries over indexed code."
    )]
    async fn a2a_pattern_recursive(
        &self,
        Parameters(params): Parameters<A2aPatternRecursiveParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_pattern_recursive",
            600,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_a2a_pattern_recursive::tool_a2a_pattern_recursive(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "MSM trajectory similarity (Part B): retrieve the most similar past RLM runs to a probe \
(Move-Split-Merge distance over their step sequences) and classify whether it trends toward success or \
failure. Powers the 'learn which decomposition worked' loop."
    )]
    async fn trajectory_similarity(
        &self,
        Parameters(params): Parameters<TrajectorySimilarityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "trajectory_similarity",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_trajectory_similarity::tool_trajectory_similarity(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Stage 5d online recognition: match a partial / in-progress numeric \
trajectory ('work_item' progress-% or 'file' weekly-churn) against the live record cohort via \
Move-Split-Merge (which aligns different-length sequences), returning the nearest known \
trajectories. Feed an unfolding series for early-warning / outcome prediction."
    )]
    async fn recognize_trajectory(
        &self,
        Parameters(params): Parameters<RecognizeTrajectoryParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "recognize_trajectory",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_trajectory_similarity::tool_recognize_trajectory(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // SOTA Phase 2 — Graph algorithms
    // ========================================================================

    #[tool(
        description = "K-core decomposition (Seidman 1983, Batagelj-Zaversnik O(m) peeling). \
USE WHEN: identifying load-bearing structural backbone vs the periphery. \
Returns each file's coreness (highest k such that the file is in a k-core)."
    )]
    async fn kcore_analysis(
        &self,
        Parameters(params): Parameters<KcoreAnalysisParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "kcore_analysis",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_kcore_analysis::tool_kcore_analysis(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "K-truss decomposition (Cohen 2008): per-edge trussness via triangle support peeling. \
USE WHEN: finding cohesive dense regions and fragile single-triangle edges."
    )]
    async fn ktruss_analysis(
        &self,
        Parameters(params): Parameters<KtrussAnalysisParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "ktruss_analysis",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_ktruss_analysis::tool_ktruss_analysis(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Personalized PageRank with restart (Tong-Faloutsos-Pan ICDM 2006). \
USE WHEN: computing blast radius from a seed set — how much does each file depend on the seeds? \
Sharper than vanilla PageRank for targeted impact analysis."
    )]
    async fn personalized_pagerank(
        &self,
        Parameters(params): Parameters<PersonalizedPagerankParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "personalized_pagerank",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_personalized_pagerank::tool_personalized_pagerank(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Edge betweenness centrality (Brandes 2001 edge variant of Girvan-Newman 2002). \
USE WHEN: finding bottleneck import edges that route many shortest paths — removing them would split or stretch the dependency graph."
    )]
    async fn edge_betweenness(
        &self,
        Parameters(params): Parameters<EdgeBetweennessParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "edge_betweenness",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_edge_betweenness::tool_edge_betweenness(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Burt's structural-holes constraint (Burt 1992). \
USE WHEN: identifying broker files that bridge otherwise-disconnected neighbourhoods (low constraint = high-leverage broker). \
DO NOT USE: as a betweenness substitute — constraint measures redundancy, not paths.")]
    async fn structural_holes(
        &self,
        Parameters(params): Parameters<StructuralHolesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "structural_holes",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_structural_holes::tool_structural_holes(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Motif / graphlet census (Milo et al. Science 2002, Pržulj GDD 2007). \
USE WHEN: characterizing architecture-signature — high 030T = clean layering, high 030C = circular deps, high cliques = god-cluster."
    )]
    async fn motif_census(
        &self,
        Parameters(params): Parameters<MotifCensusParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "motif_census",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_motif_census::tool_motif_census(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Modularity-based attack vulnerability (Holme et al. PRE 2002). \
Simulates sequential file removal by chosen order (pagerank / betweenness / degree) and tracks the largest connected component. \
USE WHEN: quantifying architectural resilience against single-file outages."
    )]
    async fn attack_vulnerability(
        &self,
        Parameters(params): Parameters<AttackVulnerabilityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "attack_vulnerability",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_attack_vulnerability::tool_attack_vulnerability(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // SOTA Phase 3 — Information theory
    // ========================================================================

    #[tool(
        description = "Normalized Compression Distance (Cilibrasi-Vitányi 2005). \
NCD(x,y) ≈ 0 = clones; ≈ 1 = unrelated. Uses zstd as compressor. \
USE WHEN: clone detection across languages — catches parametric clones embedding-cosine misses."
    )]
    async fn compression_distance(
        &self,
        Parameters(params): Parameters<CompressionDistanceParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "compression_distance",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_compression_distance::tool_compression_distance(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Mutual information for git co-change. \
Sharper than Jaccard — penalizes coincidental overlap with high-frequency files. \
USE WHEN: identifying causally-coupled refactor candidates from history.")]
    async fn cochange_mutual_information(
        &self,
        Parameters(params): Parameters<CochangeMutualInformationParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "cochange_mutual_information",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_cochange_mutual_information::tool_cochange_mutual_information(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(description = "Conditional entropy of imports H(target | source). \
USE WHEN: spotting broker modules (high entropy → imports spread across many targets) vs focused dependencies.")]
    async fn import_entropy(
        &self,
        Parameters(params): Parameters<ImportEntropyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "import_entropy",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_import_entropy::tool_import_entropy(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Shannon entropy of identifier-token distribution per file (Abebe et al. ICPC 2009). \
USE WHEN: spotting naming pollution / generated code (low entropy) vs clear domain vocabulary (high)."
    )]
    async fn identifier_entropy(
        &self,
        Parameters(params): Parameters<IdentifierEntropyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "identifier_entropy",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_identifier_entropy::tool_identifier_entropy(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // SOTA Phase 4 — Evolution + quality
    // ========================================================================

    #[tool(description = "Bus factor per file (Avelino et al. ICSE 2016). \
USE WHEN: finding single points of failure in maintainership — files where one or two authors own >= 50% of lines.")]
    async fn bus_factor(
        &self,
        Parameters(params): Parameters<BusFactorParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "bus_factor",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_bus_factor::tool_bus_factor(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Knowledge-silo detection via Gini + Herfindahl on blame distribution. \
USE WHEN: identifying single-author files / concentration-of-knowledge risks."
    )]
    async fn knowledge_silos(
        &self,
        Parameters(params): Parameters<KnowledgeSilosParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "knowledge_silos",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_knowledge_silos::tool_knowledge_silos(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Ownership-coupling mismatch: files that co-change frequently but have disjoint author sets. \
USE WHEN: predicting merge-conflict-prone refactor candidates."
    )]
    async fn ownership_coupling_mismatch(
        &self,
        Parameters(params): Parameters<OwnershipCouplingMismatchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "ownership_coupling_mismatch",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_ownership_coupling_mismatch::tool_ownership_coupling_mismatch(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Doc-code drift: cosine distance between doc-chunk and code-chunk embedding centroids per directory. \
USE WHEN: finding stale documentation whose vocabulary has diverged from the code."
    )]
    async fn doc_code_drift(
        &self,
        Parameters(params): Parameters<DocCodeDriftParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "doc_code_drift",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_doc_code_drift::tool_doc_code_drift(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Test-smell detection (van Deursen et al. XP 2001; Garousi JSS 2018). \
Detects Assertion Roulette, Mystery Guest, Conditional Logic in Tests, Eager Test."
    )]
    async fn test_smells(
        &self,
        Parameters(params): Parameters<TestSmellsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "test_smells",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_test_smells::tool_test_smells(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Mutation-testing surrogate (Just et al. FSE 2014): per-file ratio of commits that change source without changing tests."
    )]
    async fn mutation_score_surrogate(
        &self,
        Parameters(params): Parameters<MutationScoreSurrogateParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "mutation_score_surrogate",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_mutation_score_surrogate::tool_mutation_score_surrogate(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Flaky-test candidates (Luo et al. FSE 2014; Lam et al. ASE 2019). \
Heuristic over commit messages mentioning flakiness/race/retry/timing near test edits."
    )]
    async fn flaky_test_candidates(
        &self,
        Parameters(params): Parameters<FlakyTestCandidatesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "flaky_test_candidates",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_flaky_test_candidates::tool_flaky_test_candidates(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    // ========================================================================
    // SOTA Phase 5 — Concurrency / safety / performance
    // ========================================================================
    #[tool(
        description = "Detect lock-acquisition sites across Rust/C++/Java/Go/Python. \
Eraser-style lockset analysis (Savage et al. TOCS 1997) audit aid."
    )]
    async fn lockset_races(
        &self,
        Parameters(params): Parameters<LocksetRacesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "lockset_races",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_lockset_races::tool_lockset_races(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Per-file `unsafe` block density (Astrauskas OOPSLA 2020). \
Concentration of unsafe in non-FFI files = review priority."
    )]
    async fn unsafe_clusters(
        &self,
        Parameters(params): Parameters<UnsafeClustersParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "unsafe_clusters",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_unsafe_clusters::tool_unsafe_clusters(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Per-function panic-leaf count (panic!/unwrap/expect/assert) from `function_metrics`. \
USE WHEN: hunting Rust library footguns that crash on unexpected input."
    )]
    async fn panic_paths(
        &self,
        Parameters(params): Parameters<PanicPathsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "panic_paths",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_panic_paths::tool_panic_paths(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Function-level centrality (PageRank / betweenness / harmonic / coreness) over the \
symbol-resolved call graph, read from `function_metrics`. \
USE WHEN: finding the load-bearing functions to read or refactor first — sharper than file-level `centrality_analysis`."
    )]
    async fn central_functions(
        &self,
        Parameters(params): Parameters<CentralFunctionsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "central_functions",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_central_functions::tool_central_functions(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Louvain communities over the call graph — cross-file functional clusters. \
USE WHEN: recovering the real modular structure and spotting concerns smeared across many files vs. the directory layout."
    )]
    async fn function_communities(
        &self,
        Parameters(params): Parameters<FunctionCommunitiesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "function_communities",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_function_communities::tool_function_communities(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "k-core decomposition over the call graph — functions in the densely interconnected \
execution core. USE WHEN: locating the tangled architectural nucleus that resists being split out."
    )]
    async fn function_kcore(
        &self,
        Parameters(params): Parameters<FunctionKcoreParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "function_kcore",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_function_kcore::tool_function_kcore(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Mutual- and direct-recursion in the call graph (strongly-connected components of size ≥ 2 \
plus the concrete call cycles inside them). USE WHEN: finding unintended call cycles that block layering, \
complicate testing, and risk unbounded recursion."
    )]
    async fn recursive_clusters(
        &self,
        Parameters(params): Parameters<RecursiveClustersParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "recursive_clusters",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_recursive_clusters::tool_recursive_clusters(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Extended centrality (eigenvector / Katz / harmonic / closeness / reverse-PageRank) over \
the file import graph or the function call graph. USE WHEN: `centrality_analysis` (PageRank / betweenness / degree) \
isn't the right lens — influence among important neighbours (eigenvector/Katz), reach (harmonic/closeness), or \
foundational sinks everything depends on (reverse-PageRank)."
    )]
    async fn extended_centrality(
        &self,
        Parameters(params): Parameters<ExtendedCentralityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "extended_centrality",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_extended_centrality::tool_extended_centrality(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Articulation points (cut vertices) + bridges (cut edges) over the file import graph \
or the function call graph (Hopcroft-Tarjan). USE WHEN: finding true structural single points of failure — \
nodes/edges whose removal disconnects the graph — sharper than the ownership-based `bus_factor`."
    )]
    async fn articulation_points(
        &self,
        Parameters(params): Parameters<ArticulationPointsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "articulation_points",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_articulation_points::tool_articulation_points(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Connectivity & decoupling structure over the file import graph or function call \
graph: 2-edge-connected components (Tarjan — subsystems robust to single-edge failure), global min-cut \
(Stoer-Wagner — the weakest seam to split a subsystem), and Leiden well-connectedness refinement of \
Louvain (how many communities were internally disconnected). USE WHEN: assessing structural robustness or \
finding a concrete module-decoupling boundary."
    )]
    async fn graph_connectivity(
        &self,
        Parameters(params): Parameters<GraphConnectivityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "graph_connectivity",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_graph_connectivity::tool_graph_connectivity(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Chidamber-Kemerer OO metrics per class: WMC (Σ method cyclomatic), DIT (inheritance \
depth), NOC (number of children), CBO (coupling), RFC (response for class). USE WHEN: finding complex / \
deeply-inherited / heavily-coupled classes to refactor or prioritize for testing. DIT/NOC need the \
language's inherit/impl edges (Python & C/C++ emit them today)."
    )]
    async fn ck_metrics(
        &self,
        Parameters(params): Parameters<CkMetricsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "ck_metrics",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_ck_metrics::tool_ck_metrics(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Spectral analysis over the file import graph or function call graph: algebraic \
connectivity λ₂ + Fiedler bisection (Fiedler/Shi-Malik — global robustness + a balanced split boundary) \
and Weisfeiler-Lehman structural clones (repeated call/import shapes despite renamed identifiers). USE \
WHEN: gauging how bottlenecked the architecture is, finding a natural module split, or detecting \
structural (not textual) duplication."
    )]
    async fn spectral_analysis(
        &self,
        Parameters(params): Parameters<SpectralAnalysisParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "spectral_analysis",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_spectral_analysis::tool_spectral_analysis(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Design Structure Matrix metrics: propagation cost + Core/Shared/Control/Peripheral \
classification over the file import graph or function call graph (MacCormack-Rusnak-Baldwin). USE WHEN: \
quantifying overall coupling (what fraction of the system a change ripples through) and locating the \
architectural core, change-risk hubs (high visibility fan-in), and widest-blast-radius files (high fan-out)."
    )]
    async fn architecture_dsm(
        &self,
        Parameters(params): Parameters<ArchitectureDsmParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "architecture_dsm",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_architecture_dsm::tool_architecture_dsm(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Graph-aware code retrieval via Personalized PageRank (HippoRAG) over the code \
graph (import/call/co_change/semantic). USE WHEN: a query is relational ('how does X flow to Y', 'what \
configures Z') — PPR pulls in callers/callees/config one or two hops from the lexical hits that flat \
`semantic_search` / `hybrid_search` miss, in one shot."
    )]
    async fn code_ppr_search(
        &self,
        Parameters(params): Parameters<CodePprSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "code_ppr_search",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_code_ppr_search::tool_code_ppr_search(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "PathRAG over the code graph: ranked, flow-pruned dependency ROUTES (import/call/\
co_change/semantic chains) from the query's dense-similar files. USE WHEN: tracing 'how does A reach B' \
or the strongest chain linking a query hit to related code — returns the actual paths, where \
`code_ppr_search` returns ranked files."
    )]
    async fn code_path_search(
        &self,
        Parameters(params): Parameters<CodePathSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "code_path_search",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_code_path_search::tool_code_path_search(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "RAPTOR-over-code: query precomputed conceptual cluster summaries (module gists). \
USE WHEN: a query is conceptual ('where does this project handle retries', 'which module owns auth') and \
no single chunk answers it; omit `project` to compare modules across ALL indexed projects. Requires the \
`code-raptor` cron to have run."
    )]
    async fn code_raptor_search(
        &self,
        Parameters(params): Parameters<CodeRaptorSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "code_raptor_search",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_code_raptor_search::tool_code_raptor_search(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "HITS hubs & authorities over the file import graph or the function call graph \
(Kleinberg). USE WHEN: separating orchestrators (hubs) from core utilities (authorities) — a split \
PageRank conflates into one score."
    )]
    async fn hits(
        &self,
        Parameters(params): Parameters<HitsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "hits",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_hits::tool_hits(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Dominator-tree chokepoints from an entry node over the file import or function call \
graph (Cooper-Harvey-Kennedy). USE WHEN: finding must-pass-through funnels — nodes every path from the \
root traverses — to place caching/validation/boundaries or assess blast radius."
    )]
    async fn dominator_tree(
        &self,
        Parameters(params): Parameters<DominatorTreeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "dominator_tree",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_dominator_tree::tool_dominator_tree(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Lock-order cycles (Havender 1968) by scanning function bodies for lock(A);lock(B) sequences and computing SCCs."
    )]
    async fn deadlock_candidates(
        &self,
        Parameters(params): Parameters<DeadlockCandidatesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "deadlock_candidates",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_deadlock_candidates::tool_deadlock_candidates(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Rust Send/Sync violation candidates: Arc<RefCell>, static mut, unsafe Send/Sync impls."
    )]
    async fn send_sync_violations(
        &self,
        Parameters(params): Parameters<SendSyncViolationsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "send_sync_violations",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_send_sync_violations::tool_send_sync_violations(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Accidentally-quadratic loops (Petrashko ICSE 2017): for/while loops with .contains/.find/.indexOf in the body."
    )]
    async fn quadratic_loops(
        &self,
        Parameters(params): Parameters<QuadraticLoopsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "quadratic_loops",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_quadratic_loops::tool_quadratic_loops(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Missing-preallocation hotspots: Vec::new/HashMap::new without with_capacity."
    )]
    async fn missing_preallocation(
        &self,
        Parameters(params): Parameters<MissingPreallocationParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "missing_preallocation",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_missing_preallocation::tool_missing_preallocation(
                self.ctx(),
                params,
            ),
        )
        .await
    }
    #[tool(
        description = "Blocking calls inside async fn bodies (std::fs / std::sync::Mutex / time.sleep). \
Tokio anti-pattern: blocks the executor."
    )]
    async fn blocking_in_async(
        &self,
        Parameters(params): Parameters<BlockingInAsyncParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "blocking_in_async",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_blocking_in_async::tool_blocking_in_async(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = ".clone() / Arc::clone density per file × PageRank — surfaces allocation hotspots before profiling."
    )]
    async fn clone_density(
        &self,
        Parameters(params): Parameters<CloneDensityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "clone_density",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_clone_density::tool_clone_density(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "I/O calls weighted by PageRank + betweenness — finds blocking I/O on hot paths."
    )]
    async fn io_hotpath(
        &self,
        Parameters(params): Parameters<IoHotpathParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "io_hotpath",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_io_hotpath::tool_io_hotpath(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // SOTA Phase 6 — Security
    // ========================================================================
    #[tool(
        description = "Surface co-located taint sources (env/request/stdin) and sinks (exec/eval/SQL) per file. \
Newsome-Song NDSS 2005 audit aid."
    )]
    async fn taint_analysis(
        &self,
        Parameters(params): Parameters<TaintAnalysisParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "taint_analysis",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_taint_analysis::tool_taint_analysis(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Hardcoded-secret detection via entropy + known-prefix regex (Meli et al. NDSS 2019)."
    )]
    async fn secret_detection(
        &self,
        Parameters(params): Parameters<SecretDetectionParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "secret_detection",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_secret_detection::tool_secret_detection(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Crypto-misuse rules (CryptoLint CCS 2013): ECB mode, MD5/SHA-1 in auth, weak RNG for tokens, static IVs, hardcoded keys."
    )]
    async fn crypto_misuse(
        &self,
        Parameters(params): Parameters<CryptoMisuseParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "crypto_misuse",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_crypto_misuse::tool_crypto_misuse(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "CWE-502 unsafe-deserialization patterns: pickle.loads, yaml.load, ObjectInputStream, etc."
    )]
    async fn unsafe_deserialization(
        &self,
        Parameters(params): Parameters<UnsafeDeserializationParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "unsafe_deserialization",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_unsafe_deserialization::tool_unsafe_deserialization(
                self.ctx(),
                params,
            ),
        )
        .await
    }
    #[tool(
        description = "SQL / shell injection candidates: string concatenation into exec/query calls."
    )]
    async fn injection_candidates(
        &self,
        Parameters(params): Parameters<InjectionCandidatesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "injection_candidates",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_injection_candidates::tool_injection_candidates(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Mutating HTTP routes (POST/PUT/DELETE/PATCH) in files lacking visible auth middleware."
    )]
    async fn unprotected_routes(
        &self,
        Parameters(params): Parameters<UnprotectedRoutesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "unprotected_routes",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_unprotected_routes::tool_unprotected_routes(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Parse Cargo.lock / package-lock.json / requirements.txt and surface dependencies for OSV.dev review."
    )]
    async fn cve_supply_chain(
        &self,
        Parameters(params): Parameters<CveSupplyChainParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "cve_supply_chain",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_cve_supply_chain::tool_cve_supply_chain(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // SOTA Phase 7 — API / contract
    // ========================================================================
    #[tool(
        description = "Enumerate public symbols from `file_symbols.visibility='public'`. Per-kind counts (default) or full list."
    )]
    async fn public_api_surface(
        &self,
        Parameters(params): Parameters<PublicApiSurfaceParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "public_api_surface",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_public_api_surface::tool_public_api_surface(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Semver-break audit: public symbols seen in recent git history but missing from the current public API. Likely renames flagged by Levenshtein."
    )]
    async fn semver_break_audit(
        &self,
        Parameters(params): Parameters<SemverBreakAuditParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "semver_break_audit",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_semver_break_audit::tool_semver_break_audit(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Symbols annotated as deprecated but still called from inside the project. Migrate then delete."
    )]
    async fn deprecated_but_used(
        &self,
        Parameters(params): Parameters<DeprecatedButUsedParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "deprecated_but_used",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_deprecated_but_used::tool_deprecated_but_used(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "API stability score per public symbol (Bogart EMSE 2016) — change-rate over recent commits."
    )]
    async fn api_stability(
        &self,
        Parameters(params): Parameters<ApiStabilityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "api_stability",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_api_stability::tool_api_stability(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // SOTA Phase 8 — ML / embedding-based
    // ========================================================================
    #[tool(
        description = "LSH clone detection on 384-d embeddings (Indyk-Motwani STOC 1998). SimHash + banded LSH gives O(1) candidate retrieval; rerank by exact cosine."
    )]
    async fn lsh_clone_detection(
        &self,
        Parameters(params): Parameters<LshCloneDetectionParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "lsh_clone_detection",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_lsh_clone_detection::tool_lsh_clone_detection(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Semantic drift per file: cosine distance between current centroid and 30+-day historical centroid (Hamilton et al. ACL 2016)."
    )]
    async fn semantic_drift(
        &self,
        Parameters(params): Parameters<SemanticDriftParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "semantic_drift",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_semantic_drift::tool_semantic_drift(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "LOF-style embedding outliers (Breunig et al. SIGMOD 2000): chunks whose mean k-NN cosine distance is unusually high."
    )]
    async fn embedding_outliers(
        &self,
        Parameters(params): Parameters<EmbeddingOutliersParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "embedding_outliers",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_embedding_outliers::tool_embedding_outliers(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Multi-resolution PageRank: PageRank within each Louvain community + PageRank on the community-supernode graph. Surfaces both module leaders and module importance."
    )]
    async fn multi_resolution_pagerank(
        &self,
        Parameters(params): Parameters<MultiResolutionPagerankParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "multi_resolution_pagerank",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_multi_resolution_pagerank::tool_multi_resolution_pagerank(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    // ========================================================================
    // SOTA Phase 9 — Data engineering
    // ========================================================================
    #[tool(
        description = "Migration-safety audit (Strong-Migrations + Curino VLDB 2008): DROP/ALTER, non-CONCURRENT index, NOT NULL without default."
    )]
    async fn migration_safety(
        &self,
        Parameters(params): Parameters<MigrationSafetyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "migration_safety",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_migration_safety::tool_migration_safety(self.ctx(), params),
        )
        .await
    }
    #[tool(description = "Columns declared in SQL DDL but never referenced anywhere in source.")]
    async fn dead_columns(
        &self,
        Parameters(params): Parameters<DeadColumnsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "dead_columns",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_dead_columns::tool_dead_columns(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "PII detection: PII-shaped literals + PII-named identifiers co-located with logging or network sinks."
    )]
    async fn pii_spread(
        &self,
        Parameters(params): Parameters<PiiSpreadParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "pii_spread",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_pii_spread::tool_pii_spread(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // SOTA Phase 10 — Call-graph downstream
    // ========================================================================
    #[tool(
        description = "Forward reachability dead-code: symbols unreached from main / public exports / test entry points."
    )]
    async fn dead_code_reachability(
        &self,
        Parameters(params): Parameters<DeadCodeReachabilityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "dead_code_reachability",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_dead_code_reachability::tool_dead_code_reachability(
                self.ctx(),
                params,
            ),
        )
        .await
    }
    #[tool(
        description = "Feature envy (Lanza-Marinescu 2006): functions whose external-data references dominate own-file references."
    )]
    async fn feature_envy(
        &self,
        Parameters(params): Parameters<FeatureEnvyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "feature_envy",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_feature_envy::tool_feature_envy(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Shotgun-surgery detection from git history: commits touching many files indicate scattered responsibility."
    )]
    async fn shotgun_surgery(
        &self,
        Parameters(params): Parameters<ShotgunSurgeryParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "shotgun_surgery",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_shotgun_surgery::tool_shotgun_surgery(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "LCOM4 (Hitz-Montazeri 1995): per-container connected components in the member-method shared-target graph."
    )]
    async fn lcom4(
        &self,
        Parameters(params): Parameters<Lcom4Params>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "lcom4",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_lcom4::tool_lcom4(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // SOTA Phase 11 — Evolution analytics
    // ========================================================================
    #[tool(
        description = "Refactor pressure (Tufano ICSE 2015): per-file ratio of non-test commits to test commits in the window."
    )]
    async fn refactor_pressure(
        &self,
        Parameters(params): Parameters<RefactorPressureParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "refactor_pressure",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_refactor_pressure::tool_refactor_pressure(self.ctx(), params),
        )
        .await
    }
    #[tool(description = "Page CUSUM change-points on per-file commit rate (weekly).")]
    async fn commit_changepoint(
        &self,
        Parameters(params): Parameters<CommitChangepointParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "commit_changepoint",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_commit_changepoint::tool_commit_changepoint(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Per-file commit-message vocabulary drift via Porter-stemmed TF cosine across sliding windows."
    )]
    async fn commit_topic_drift(
        &self,
        Parameters(params): Parameters<CommitTopicDriftParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "commit_topic_drift",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_commit_topic_drift::tool_commit_topic_drift(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "API stability scored over release-marker commits (Bogart EMSE 2016 adapted)."
    )]
    async fn release_api_stability(
        &self,
        Parameters(params): Parameters<ReleaseApiStabilityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "release_api_stability",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_release_api_stability::tool_release_api_stability(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    // ========================================================================
    // Phase 5: NLP & IR tools
    // ========================================================================

    #[tool(
        description = "Combined keyword + semantic search using Reciprocal Rank Fusion (RRF). \
Runs BM25 full-text and vector similarity in parallel, merges with configurable weights. \
USE WHEN: query is partially lexical and partially conceptual ('async error handling'), \
or you want robust ranking when neither pure keyword nor pure semantic alone gets the \
right top result. \
DO NOT USE WHEN: query is purely lexical (text_search is sufficient) or purely \
conceptual (semantic_search is sufficient). \
RRF gives more stable ordering than either branch alone for mixed queries."
    )]
    async fn hybrid_search(
        &self,
        Parameters(params): Parameters<HybridSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "hybrid_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_hybrid_search::tool_hybrid_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Structural summary of a project, directory, or specific file. \
USE WHEN: writing a module's README, explaining unfamiliar code to someone, or generating \
a design-doc starting point. Combines PageRank-ranked key modules + topic assignments + \
language breakdown into prose. \
DO NOT USE WHEN: you only need a directory listing — use `project_tree`. \
Requires graph-analysis cron and discover_topics. The `orient` tool gives a faster \
project-wide overview without prose."
    )]
    async fn code_summarize(
        &self,
        Parameters(params): Parameters<CodeSummarizeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "code_summarize",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_code_summarize::tool_code_summarize(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // Phase 6: Engineering Scorecard
    // ========================================================================

    #[tool(
        description = "Engineering-quality scorecard: 10 dimensions A-F + GPA + ORR checklist. \
USE WHEN: producing a quarterly health report for a service, evaluating whether a project \
is ready for production handoff, or comparing the maturity of two projects. \
DO NOT USE WHEN: you only need a single dimension — call the underlying tool directly \
(`architecture_quality`, `bug_prediction`, `test_coverage_gaps`, etc.). \
Aggregates dependency analysis + architecture quality + design smells + test/doc coverage \
+ health metrics. Requires graph-analysis cron + discover_topics."
    )]
    async fn engineering_scorecard(
        &self,
        Parameters(params): Parameters<EngineeringScorecardParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "engineering_scorecard",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_engineering_scorecard::tool_engineering_scorecard(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    // ========================================================================
    // Phase D2b — new shadow-ASR-native tools (6)
    // ========================================================================

    #[tool(
        description = "Find functions in different languages with matching signature shape. \
USE WHEN: validating cross-language ports (MeTTa→Rholang→Rust), auditing whether a compiler \
preserved semantics, or harmonizing APIs across language SDKs. \
Reads from the materialized `cross_language_signature_clones` table — call `trigger_cron` \
with `cross_language_signatures` to refresh."
    )]
    async fn cross_language_api_equivalents(
        &self,
        Parameters(params): Parameters<CrossLanguageApiEquivalentsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "cross_language_api_equivalents",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_cross_language_api_equivalents::tool_cross_language_api_equivalents(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Search for functions by structural type shape: return type tags, \
parameter type tags, effects. USE WHEN: 'find async functions returning Result<T,_>', \
'all handlers taking Request<_>', 'database-touching functions in module foo'. \
Backed by GIN-indexed `return_type_tags` and `symbol_parameters.type_tags`."
    )]
    async fn type_shape_search(
        &self,
        Parameters(params): Parameters<TypeShapeSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "type_shape_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_type_shape_search::tool_type_shape_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Find call sites of a target path, filtered by the caller's signature \
shape (e.g. 'callers whose parameter N has type-tag Mutex'). USE WHEN: scoping \
a refactor that touches all callers carrying a specific type, or locating \
callers in a specific effect-set context."
    )]
    async fn find_callers_by_signature(
        &self,
        Parameters(params): Parameters<FindCallersBySignatureParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "find_callers_by_signature",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_find_callers_by_signature::tool_find_callers_by_signature(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Forward or reverse effect closure along resolved call edges. \
Forward (seed_symbol_id): which effects are reached from this symbol? \
Reverse (target_effects): which symbols reach any of these effects? \
USE WHEN: tracing 'what touches network?', 'who could reach gpu_kernel?', \
or 'what does this entry point ultimately do?'."
    )]
    async fn effect_propagation(
        &self,
        Parameters(params): Parameters<EffectPropagationParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "effect_propagation",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_effect_propagation::tool_effect_propagation(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "List the type-tag and effect vocabularies with per-tag usage counts \
and descriptions. USE WHEN: orienting to the tag schema before formulating \
queries, or auditing which tags actually appear in this project's code."
    )]
    async fn type_tag_dictionary(
        &self,
        Parameters(params): Parameters<TypeTagDictionaryParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "type_tag_dictionary",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_type_tag_dictionary::tool_type_tag_dictionary(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Programming-paradigm profile of source code. Runs libgrammstein's \
ParadigmDetector (regex-heuristic) and returns OOP / FP / Reactive / Procedural \
weights plus the dominant paradigm. USE WHEN: characterizing a file's style, \
detecting paradigm drift across a project, or sanity-checking an architectural \
review. Pure code analysis — no DB access."
    )]
    async fn paradigm_profile(
        &self,
        Parameters(params): Parameters<ParadigmProfileParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "paradigm_profile",
            10,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_paradigm_profile::tool_paradigm_profile(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Build a Code Property Graph (AST ∪ CFG ∪ DFG) for source code.")]
    async fn code_property_graph(
        &self,
        Parameters(params): Parameters<CodePropertyGraphParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "code_property_graph",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_code_property_graph::run(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Frequent-subtree mining (TreeminerD) across a list of source strings.")]
    async fn subtree_mining(
        &self,
        Parameters(params): Parameters<SubtreeMiningParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "subtree_mining",
            60,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_subtree_mining::run(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Normalize a term via liblevenshtein's phonetic framework.")]
    async fn phonetic_normalize(
        &self,
        Parameters(params): Parameters<PhoneticNormalizeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "phonetic_normalize",
            5,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_phonetic_normalize::run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Reverse-expand a query into a phonetic regex (e.g. nite → (n|kn)i(t|te|ght))."
    )]
    async fn expand_query_to_phonetic_pattern(
        &self,
        Parameters(params): Parameters<ExpandQueryToPhoneticPatternParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "expand_query_to_phonetic_pattern",
            5,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_expand_query_to_phonetic_pattern::run(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Articulatory edit distance between two strings (IPA-feature weighted).")]
    async fn articulatory_distance(
        &self,
        Parameters(params): Parameters<ArticulatoryDistanceParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "articulatory_distance",
            2,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_articulatory_distance::run(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Read the persisted dendrogram-topic-hierarchy for a project (Phase 7).")]
    async fn dendrogram_topic_hierarchy(
        &self,
        Parameters(params): Parameters<DendrogramTopicHierarchyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "dendrogram_topic_hierarchy",
            10,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_dendrogram_topic_hierarchy::run(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Fuzzy symbol search via Damerau-Levenshtein over a candidate set.")]
    async fn fuzzy_symbol_search(
        &self,
        Parameters(params): Parameters<FuzzySymbolSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "fuzzy_symbol_search",
            5,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_fuzzy_symbol_search::run(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Fuzzy path search via Damerau-Levenshtein over indexed file paths.")]
    async fn fuzzy_path_search(
        &self,
        Parameters(params): Parameters<FuzzyPathSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "fuzzy_path_search",
            5,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_fuzzy_path_search::run(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Substring search via suffix automaton (exact, fast).")]
    async fn substring_search(
        &self,
        Parameters(params): Parameters<SubstringSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "substring_search",
            5,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_substring_search::run(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Per-token fuzzy grep via liblevenshtein's TokenGrep semantics.")]
    async fn token_grep(
        &self,
        Parameters(params): Parameters<TokenGrepParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "token_grep",
            5,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_token_grep::run(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Time-series fuzzy match via MSM distance (commit-cadence patterns).")]
    async fn time_series_fuzzy_match(
        &self,
        Parameters(params): Parameters<TimeSeriesFuzzyMatchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "time_series_fuzzy_match",
            5,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_time_series_fuzzy_match::run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Correct a user query against a project's persistent symbol vocabulary using pgmcp's WFST corrector: per-token Damerau-Levenshtein candidates from the symbol trie, an edit+phonetic-cost correction lattice, and (when a trained per-project model exists) Modified-Kneser-Ney LM rescoring. Returns corrected text, changed flag, confidence, and used_lm. Params: query, project, max_distance (default 2), lm_weight (default 0.5)."
    )]
    async fn correct_query(
        &self,
        Parameters(params): Parameters<CorrectQueryParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "correct_query",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_correct_query::run(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "In-process mandate dedup via Damerau-Levenshtein over an active set.")]
    async fn mandate_dedup_v2(
        &self,
        Parameters(params): Parameters<MandateDedupV2Params>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "mandate_dedup_v2",
            5,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_mandate_dedup_v2::run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Positional fuzzy grep over a caller-supplied haystack via liblevenshtein TokenGrep: matches the query approximately at every position (byte spans + edit distance), not as whole-line dictionary terms. For approximate search across the INDEXED codebase, use `grep` with fuzzy=true."
    )]
    async fn fuzzy_grep(
        &self,
        Parameters(params): Parameters<FuzzyGrepParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "fuzzy_grep",
            5,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_fuzzy_grep::run(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Phonetic grep over comment/string lines (PhoneticGrepOnline).")]
    async fn phonetic_grep_comments(
        &self,
        Parameters(params): Parameters<PhoneticGrepCommentsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "phonetic_grep_comments",
            5,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_phonetic_grep_comments::run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Composed phonetic∘edit symbol search over a project's persistent symbol trie (liblevenshtein PhoneticNormalizedDictionary): normalizes query+terms with the active phonetic rules, matches within max_distance in normalized space, returns symbols with kind/visibility/file_id/line ranked by edit then articulatory distance. Params: query, project, max_distance (default 2), limit (default 20)."
    )]
    async fn phonetic_symbol_search(
        &self,
        Parameters(params): Parameters<PhoneticSymbolSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "phonetic_symbol_search",
            5,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_phonetic_symbol_search::run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Phonetic naming consistency: identifiers that share a phonetic class but differ in spelling."
    )]
    async fn phonetic_naming_consistency(
        &self,
        Parameters(params): Parameters<PhoneticNamingConsistencyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "phonetic_naming_consistency",
            5,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_phonetic_naming_consistency::run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Articulatory naming consistency: identifiers within an articulatory-distance threshold."
    )]
    async fn articulatory_naming_consistency(
        &self,
        Parameters(params): Parameters<ArticulatoryNamingConsistencyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "articulatory_naming_consistency",
            5,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_articulatory_naming_consistency::run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Rename oracle: pick the most-likely current-day rename for a removed symbol."
    )]
    async fn rename_oracle(
        &self,
        Parameters(params): Parameters<RenameOracleParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "rename_oracle",
            5,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_rename_oracle::run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Semantic-issue scan over a Code Property Graph via libgrammstein's \
GnnSemanticScorer. Returns SemanticIssue records (unused bindings, suspicious DFG \
patterns, etc.) detected by the GNN's heuristic-edge-walk detection — no neural \
inference dependency required (the `code-neural` feature gates a placeholder \
inference path that the current upstream heuristic detector doesn't use)."
    )]
    async fn gnn_semantic_issues(
        &self,
        Parameters(params): Parameters<GnnSemanticIssuesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "gnn_semantic_issues",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_gnn_semantic_issues::run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Lint same-shape APIs for signature smells: long parameter lists, \
primitive obsession, boolean-flag explosion, inconsistent parameter naming across \
functions sharing the same shape. Returns categorized findings; consumers can act \
on each category independently."
    )]
    async fn signature_lint(
        &self,
        Parameters(params): Parameters<SignatureLintParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "signature_lint",
            30,
            &_ctx,
            &summarize_debug(&params),
            super::tools::tool_signature_lint::tool_signature_lint(self.ctx(), params),
        )
        .await
    }
}

// ============================================================================
// Agglomerative clustering for topic hierarchy (ndarray-accelerated)
// ============================================================================

/// Agglomerative clustering with average linkage on topic centroids.
///
/// Pairwise cosine similarities are computed as a single matrix multiplication
/// `sim = C × Cᵀ` using ndarray, which is orders of magnitude faster than
/// element-wise loops (exploits SIMD and cache-friendly memory access).
///
/// Returns (groups, dendrogram).
pub(crate) fn agglomerative_cluster(
    centroids: &[&[f32]],
    labels: &[String],
    sizes: &[i64],
    topic_ids: &[i32],
    num_groups: usize,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    use ndarray::Array2;

    let n = centroids.len();
    let dim = centroids[0].len();

    // Build centroid matrix (n × dim) as f64 for precision
    let mut centroid_matrix = Array2::<f64>::zeros((n, dim));
    for (i, centroid) in centroids.iter().enumerate() {
        for (j, &val) in centroid.iter().enumerate() {
            centroid_matrix[[i, j]] = val as f64;
        }
    }

    // Compute full pairwise cosine similarity matrix via matmul: sim = C × Cᵀ
    // Since centroids are L2-normalized, dot product = cosine similarity.
    let sim_matrix = centroid_matrix.dot(&centroid_matrix.t());

    // Initialize cluster-level similarity matrix from point similarity matrix.
    // UPGMA update formula maintains this incrementally: O(k) per merge instead
    // of O(|Ci|×|Cj|) member-pair recomputation.
    let mut cluster_sim: Vec<Vec<f64>> = (0..n)
        .map(|i| (0..n).map(|j| sim_matrix[[i, j]]).collect())
        .collect();
    let mut cluster_sizes: Vec<usize> = vec![1; n];

    let mut cluster_members: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();
    let mut dendrogram: Vec<serde_json::Value> = Vec::new();

    // Active index list: avoids scanning deactivated indices every iteration
    let mut active_indices: Vec<usize> = (0..n).collect();
    let mut step = 0;

    while active_indices.len() > num_groups {
        // Find the most similar pair among active clusters
        let mut best_sim = f64::NEG_INFINITY;
        let mut best_i = 0;
        let mut best_j = 0;

        for (ai, &i) in active_indices.iter().enumerate() {
            for &j in &active_indices[ai + 1..] {
                if cluster_sim[i][j] > best_sim {
                    best_sim = cluster_sim[i][j];
                    best_i = i;
                    best_j = j;
                }
            }
        }

        // Record dendrogram step
        step += 1;
        let all_merged: Vec<&str> = cluster_members[best_i]
            .iter()
            .chain(cluster_members[best_j].iter())
            .map(|&idx| labels[idx].as_str())
            .collect();

        dendrogram.push(serde_json::json!({
            "step": step,
            "merged": all_merged,
            "distance": format!("{:.4}", 1.0 - best_sim),
        }));

        // UPGMA update: recompute cluster_sim[best_i][k] for all active k
        let size_a = cluster_sizes[best_i];
        let size_b = cluster_sizes[best_j];
        let total = size_a + size_b;
        for &k in &active_indices {
            if k == best_i || k == best_j {
                continue;
            }
            let new_sim = (size_a as f64 * cluster_sim[best_i][k]
                + size_b as f64 * cluster_sim[best_j][k])
                / total as f64;
            cluster_sim[best_i][k] = new_sim;
            cluster_sim[k][best_i] = new_sim;
        }
        cluster_sizes[best_i] = total;

        // Merge cluster best_j into best_i
        let members_j = cluster_members[best_j].clone();
        cluster_members[best_i].extend(members_j);

        // Remove best_j from active indices
        active_indices.retain(|&x| x != best_j);
    }

    // Build output groups from remaining active clusters
    let mut groups: Vec<serde_json::Value> = Vec::new();
    for &ci in &active_indices {
        let members = &cluster_members[ci];

        let group_topics: Vec<serde_json::Value> = members
            .iter()
            .map(|&idx| {
                serde_json::json!({
                    "id": topic_ids[idx],
                    "label": labels[idx],
                    "size": sizes[idx],
                })
            })
            .collect();

        // Group label: join topic labels with " + "
        let group_label = members
            .iter()
            .map(|&idx| labels[idx].as_str())
            .collect::<Vec<_>>()
            .join(" + ");

        // Average internal distance from precomputed point-level sim_matrix
        let mut internal_sum = 0.0f64;
        let mut internal_count = 0usize;
        for i in 0..members.len() {
            for j in (i + 1)..members.len() {
                internal_sum += 1.0 - sim_matrix[[members[i], members[j]]];
                internal_count += 1;
            }
        }
        let avg_distance = if internal_count > 0 {
            internal_sum / internal_count as f64
        } else {
            0.0
        };

        groups.push(serde_json::json!({
            "group_label": group_label,
            "merge_distance": format!("{:.4}", avg_distance),
            "topic_count": members.len(),
            "topics": group_topics,
        }));
    }

    // Sort groups by size descending
    groups.sort_by(|a, b| {
        let sa = a["topic_count"].as_u64().unwrap_or(0);
        let sb = b["topic_count"].as_u64().unwrap_or(0);
        sb.cmp(&sa)
    });

    (groups, dendrogram)
}

/// Format a ClusteringSummary into the JSON response structure.
pub(crate) fn format_clustering_summary(
    summary: &crate::cron::topic_clustering::ClusteringSummary,
    limit: i32,
) -> serde_json::Value {
    let noise_pct = if summary.chunks_analyzed > 0 {
        summary.noise_chunks as f64 / summary.chunks_analyzed as f64 * 100.0
    } else {
        0.0
    };

    let topics: Vec<serde_json::Value> = summary.topics.iter().take(limit as usize).map(|t| {
        serde_json::json!({
            "id": t.cluster_index,
            "label": t.label,
            "keywords": t.keywords,
            "keyword_scores": t.keyword_scores.iter().map(|s| format!("{:.4}", s)).collect::<Vec<_>>(),
            "size": t.chunk_ids.len(),
            "files": t.file_ids.len(),
            "projects": t.project_names,
            "project_count": t.project_names.len(),
            "avg_internal_similarity": format!("{:.4}", t.avg_internal_similarity),
            "representative_files": t.top_files.iter().take(10).map(|f| serde_json::json!({
                "path": f.path,
                "project": f.project,
                "chunks": f.chunks_in_topic,
            })).collect::<Vec<_>>(),
            "representative_snippet": truncate(&t.representative_snippet, 500),
        })
    }).collect();

    serde_json::json!({
        "scope": summary.scope,
        "algorithm": "Fuzzy C-Means + c-TF-IDF",
        "params": {
            "num_clusters": summary.num_clusters,
            "fuzziness": summary.fuzziness,
            "converged": summary.converged,
            "iterations": summary.iterations,
        },
        "chunks_analyzed": summary.chunks_analyzed,
        "topics_found": summary.topics_found,
        "noise_chunks": summary.noise_chunks,
        "noise_pct": format!("{:.1}", noise_pct),
        "topics": topics,
        "guidance": "Use compare_files to examine specific file pairs within a topic. \
                     Topics with high avg_internal_similarity and multiple files indicate \
                     DRY candidates. Keywords show c-TF-IDF extracted semantic labels.",
    })
}

// ============================================================================
// CLI dispatch — call tool handlers without a running MCP session
// ============================================================================

/// CLI dispatch by tool name. Bypasses the rmcp `#[tool]` wrapper (which
/// now requires a `RequestContext<RoleServer>` synthesized by the transport
/// layer) and invokes the underlying tool body directly. The CLI test path
/// has no MCP peer so telemetry attribution records `client = "unknown"` for
/// these calls.
///
/// Most tools live in a module whose name matches the tool name (e.g.
/// `tool_grep::tool_grep`); for those, the default branch uses `paste!` to
/// concatenate. A second branch accepts `in $body_mod` for the handful of
/// tools whose body lives in a different module (e.g. the seven
/// software-pattern tools all share `tool_software_patterns`).
macro_rules! dispatch_tool {
    ($self:expr, $name:expr, $args:expr, {
        $($tool_name:literal => $method:ident($params_ty:ty)
            $(in $body_mod:ident)?),* $(,)?
    }, no_params: {
        $($np_name:literal => $np_method:ident
            $(in $np_body_mod:ident)?),* $(,)?
    }) => {
        match $name {
            $(
                $tool_name => {
                    let params: $params_ty = serde_json::from_value($args)
                        .map_err(|e| McpError::invalid_params(
                            format!("Invalid parameters for '{}': {}", $tool_name, e), None
                        ))?;
                    dispatch_tool_call!($self, $method, params $(, in $body_mod)?)
                }
            )*
            $(
                $np_name => dispatch_tool_call_nullary!($self, $np_method $(, in $np_body_mod)?),
            )*
            _ => Err(McpError::invalid_params(
                format!("Unknown tool: '{}'. Run `pgmcp tool` to list available tools.", $name), None
            ))
        }
    };
}

/// Helper for `dispatch_tool!`: invoke a tool body that takes a params arg.
/// Default branch infers the module name as `tool_$method`; the `in $body_mod`
/// branch overrides it.
macro_rules! dispatch_tool_call {
    ($self:expr, $method:ident, $params:expr) => {
        paste::paste! {
            super::tools::[<tool_ $method>]::[<tool_ $method>]($self.ctx(), $params).await
        }
    };
    ($self:expr, $method:ident, $params:expr, in $body_mod:ident) => {
        paste::paste! {
            super::tools::$body_mod::[<tool_ $method>]($self.ctx(), $params).await
        }
    };
}

/// Helper for `dispatch_tool!`: invoke a nullary tool body.
macro_rules! dispatch_tool_call_nullary {
    ($self:expr, $method:ident) => {
        paste::paste! {
            super::tools::[<tool_ $method>]::[<tool_ $method>]($self.ctx()).await
        }
    };
    ($self:expr, $method:ident, in $body_mod:ident) => {
        paste::paste! {
            super::tools::$body_mod::[<tool_ $method>]($self.ctx()).await
        }
    };
}

impl McpServer {
    /// Return the full tool catalog (name, description, input_schema) for all registered tools.
    /// Kept as an instance method for potential future use by daemon-mode code.
    #[allow(dead_code)] // Used only through MCP tool handlers and daemon code
    pub(crate) fn tool_catalog(&self) -> Vec<rmcp::model::Tool> {
        self.tool_router.list_all()
    }

    /// Dispatch a tool call by name + JSON args, bypassing the MCP transport layer.
    /// Intentionally `pub` (not `pub(crate)`) so external test crates
    /// (e.g. `pgmcp-testing/tests/`) can drive any MCP tool without
    /// depending on the rmcp transport layer.
    ///
    /// Routes through `instrumented_tool_run` so CLI invocations get the
    /// same per-call tracing events, in-memory `StatsTracker` counters,
    /// and durable `mcp_tool_calls` telemetry rows as the MCP transport
    /// path. Caller is identified as `client = "cli"`. No timeout is
    /// applied — the interactive user can cancel with Ctrl-C if needed.
    #[allow(dead_code)] // Used by the bin crate (src/main.rs); lib's external test consumers reach it through this.
    pub async fn call_tool_cli(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<CallToolResult, McpError> {
        let caller = CallerInfo {
            client_name: "cli".to_string(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: "n/a".to_string(),
        };
        let params_summary = summarize_json(&args);
        let fut = async move {
            dispatch_tool!(self, name, args, {
                // Search
                "semantic_search"        => semantic_search(SemanticSearchParams),
                "text_search"            => text_search(TextSearchParams),
                "grep"                   => grep(GrepParams),
                "hybrid_search"          => hybrid_search(HybridSearchParams),
                "search_commits"         => search_commits(SearchCommitsParams),
                // Pattern knowledge — all share the `tool_software_patterns` module.
                "software_pattern_search"    => software_pattern_search(SoftwarePatternSearchParams) in tool_software_patterns,
                "recommend_design_patterns"  => recommend_design_patterns(RecommendDesignPatternsParams) in tool_software_patterns,
                "review_design_patterns"     => review_design_patterns(ReviewDesignPatternsParams) in tool_software_patterns,
                "get_software_pattern"       => get_software_pattern(GetSoftwarePatternParams) in tool_software_patterns,
                "list_software_patterns"     => list_software_patterns(ListSoftwarePatternsParams) in tool_software_patterns,
                "refresh_pattern_catalog"    => refresh_pattern_catalog(RefreshPatternCatalogParams) in tool_software_patterns,
                "upsert_pattern_source"      => upsert_pattern_source(UpsertPatternSourceParams) in tool_software_patterns,
                // Session-level mandates — `promote_session_mandate` shares the `tool_session_mandates` module.
                "session_mandates"           => session_mandates(SessionMandatesParams),
                "promote_session_mandate"    => promote_session_mandate(PromoteSessionMandateParams) in tool_session_mandates,
                // Memory-server Phase 0 quick wins.
                "recall_prompts"             => recall_prompts(RecallPromptsParams),
                "search_mandates"            => search_mandates(SearchMandatesParams),
                // Memory-server Phase 3.1 official-compat CRUD (9 tools share the `tool_memory_crud` module).
                "memory_create_entities"     => memory_create_entities(MemoryCreateEntitiesParams) in tool_memory_crud,
                "memory_create_relations"    => memory_create_relations(MemoryCreateRelationsParams) in tool_memory_crud,
                "memory_add_observations"    => memory_add_observations(MemoryAddObservationsParams) in tool_memory_crud,
                "memory_delete_entities"     => memory_delete_entities(MemoryDeleteEntitiesParams) in tool_memory_crud,
                "memory_delete_observations" => memory_delete_observations(MemoryDeleteObservationsParams) in tool_memory_crud,
                "memory_delete_relations"    => memory_delete_relations(MemoryDeleteRelationsParams) in tool_memory_crud,
                "memory_read_graph"          => memory_read_graph(MemoryReadGraphParams) in tool_memory_crud,
                "memory_search_nodes"        => memory_search_nodes(MemorySearchNodesParams) in tool_memory_crud,
                "memory_open_nodes"          => memory_open_nodes(MemoryOpenNodesParams) in tool_memory_crud,
                // Memory-server Phase 3.2 extensions (share the tool_memory_ext module).
                "memory_semantic_search"        => memory_semantic_search(MemorySemanticSearchParams) in tool_memory_ext,
                "memory_hybrid_search"          => memory_hybrid_search(MemoryHybridSearchParams) in tool_memory_ext,
                "memory_facts_at"               => memory_facts_at(MemoryFactsAtParams) in tool_memory_ext,
                "memory_relations_traverse"     => memory_relations_traverse(MemoryRelationsTraverseParams) in tool_memory_ext,
                "memory_anchor_entity"          => memory_anchor_entity(MemoryAnchorEntityParams) in tool_memory_ext,
                "memory_unanchor_entity"        => memory_unanchor_entity(MemoryUnanchorEntityParams) in tool_memory_ext,
                "memory_find_code_for_entity"   => memory_find_code_for_entity(MemoryFindCodeForEntityParams) in tool_memory_ext,
                "memory_find_entities_for_code" => memory_find_entities_for_code(MemoryFindEntitiesForCodeParams) in tool_memory_ext,
                "memory_reflect"                => memory_reflect(MemoryReflectParams) in tool_memory_reflect,
                // Memory-server Phase 6 graph-enhanced retrieval.
                "memory_unified_search"         => memory_unified_search(MemoryUnifiedSearchParams) in tool_memory_graph_rag,
                "memory_neighbors"              => memory_neighbors(MemoryNeighborsParams) in tool_memory_graph_rag,
                "graph_neighbors"               => graph_neighbors(GraphNeighborsParams) in tool_memory_graph_rag,
                "memory_path_search"            => memory_path_search(MemoryPathSearchParams) in tool_memory_graph_rag,
                "memory_ppr_search"             => memory_ppr_search(MemoryPprSearchParams) in tool_memory_graph_rag,
                "memory_raptor_search"          => memory_raptor_search(MemoryRaptorSearchParams) in tool_memory_graph_rag,
                // Memory-server Phase 8: forget + retention.
                "memory_forget"                 => memory_forget(MemoryForgetParams) in tool_memory_forget,
                "memory_purge_expired"          => memory_purge_expired(MemoryPurgeExpiredParams) in tool_memory_forget,
                // Memory-server Phase 10: client-profile introspection.
                "pgmcp_client_profile"          => pgmcp_client_profile(PgmcpClientProfileParams) in tool_client_profile,
                // File info
                "read_file"              => read_file(ReadFileParams),
                "mandate_context"        => mandate_context(MandateContextParams),
                "project_tree"           => project_tree(ProjectTreeParams),
                "file_info"              => file_info(FileInfoParams),
                // Similarity
                "compare_files"          => compare_files(CompareFilesParams),
                "find_similar_modules"   => find_similar_modules(FindSimilarModulesParams),
                "find_duplicates"        => find_duplicates(FindDuplicatesParams),
                "refactoring_report"     => refactoring_report(RefactoringReportParams),
                // Topics
                "discover_topics"        => discover_topics(DiscoverTopicsParams),
                "find_orphans"           => find_orphans(FindOrphansParams),
                "find_misplaced_code"    => find_misplaced_code(FindMisplacedCodeParams),
                "find_coupled_files"     => find_coupled_files(FindCoupledFilesParams),
                "test_coverage_gaps"     => test_coverage_gaps(TestCoverageGapsParams),
                "complexity_hotspots"    => complexity_hotspots(ComplexityHotspotsParams),
                "topic_hierarchy"        => topic_hierarchy(TopicHierarchyParams),
                "suggest_merges"         => suggest_merges(SuggestMergesParams),
                "suggest_splits"         => suggest_splits(SuggestSplitsParams),
                "doc_coverage_gaps"      => doc_coverage_gaps(DocCoverageGapsParams),
                // Graph
                "dependency_graph"       => dependency_graph(DependencyGraphParams),
                "centrality_analysis"    => centrality_analysis(CentralityAnalysisParams),
                "community_detection"    => community_detection(CommunityDetectionParams),
                "circular_dependencies"  => circular_dependencies(CircularDependenciesParams),
                "change_impact_analysis" => change_impact_analysis(ChangeImpactAnalysisParams),
                // Graph-roadmap Phase 1.1 — function-level call-graph analytics
                "central_functions"      => central_functions(CentralFunctionsParams),
                "function_communities"   => function_communities(FunctionCommunitiesParams),
                "function_kcore"         => function_kcore(FunctionKcoreParams),
                "recursive_clusters"     => recursive_clusters(RecursiveClustersParams),
                "extended_centrality"    => extended_centrality(ExtendedCentralityParams),
                "articulation_points"    => articulation_points(ArticulationPointsParams),
                "hits"                   => hits(HitsParams),
                "dominator_tree"         => dominator_tree(DominatorTreeParams),
                // Graph-roadmap Phase 3-4 — connectivity / spectral / DSM / CK /
                // graph-aware retrieval (file or call graph).
                "graph_connectivity"     => graph_connectivity(GraphConnectivityParams),
                "spectral_analysis"      => spectral_analysis(SpectralAnalysisParams),
                "architecture_dsm"       => architecture_dsm(ArchitectureDsmParams),
                "ck_metrics"             => ck_metrics(CkMetricsParams),
                "code_ppr_search"        => code_ppr_search(CodePprSearchParams),
                "code_path_search"       => code_path_search(CodePathSearchParams),
                "code_raptor_search"     => code_raptor_search(CodeRaptorSearchParams),
                // Architecture
                "coupling_cohesion_report"  => coupling_cohesion_report(CouplingCohesionReportParams),
                "architecture_violations"   => architecture_violations(ArchitectureViolationsParams),
                "design_smell_detection"    => design_smell_detection(DesignSmellDetectionParams),
                "architecture_quality"      => architecture_quality(ArchitectureQualityParams),
                "design_metrics"            => design_metrics(DesignMetricsParams),
                // Prediction
                "bug_prediction"         => bug_prediction(BugPredictionParams),
                "technical_debt_analysis" => technical_debt_analysis(TechnicalDebtAnalysisParams),
                "anomaly_detection"      => anomaly_detection(AnomalyDetectionParams),
                "code_on_fire"           => code_on_fire(CodeOnFireParams),
                "documented_tech_debt"   => documented_tech_debt(DocumentedTechDebtParams),
                "trigger_cron"           => trigger_cron(TriggerCronParams),
                // A2A inter-agent IPC bridge
                "a2a_send_task"          => a2a_send_task(A2aSendTaskParams),
                "a2a_get_task"           => a2a_get_task(A2aGetTaskParams),
                "a2a_subscribe_task"     => a2a_subscribe_task(A2aSubscribeTaskParams),
                "a2a_cancel_task"        => a2a_cancel_task(A2aCancelTaskParams),
                "a2a_register_agent"     => a2a_register_agent(A2aRegisterAgentParams),
                "a2a_list_agents"        => a2a_list_agents(A2aListAgentsParams),
                // A2A RecursiveMAS-inspired extensions
                "a2a_find_agents_by_specialty"
                    => a2a_find_agents_by_specialty(A2aFindAgentsBySpecialtyParams),
                "a2a_pattern_sequential" => a2a_pattern_sequential(A2aPatternSequentialParams),
                "a2a_pattern_mixture"    => a2a_pattern_mixture(A2aPatternMixtureParams),
                "a2a_pattern_distillation"
                    => a2a_pattern_distillation(A2aPatternDistillationParams),
                "a2a_pattern_deliberation"
                    => a2a_pattern_deliberation(A2aPatternDeliberationParams),
                // CSM / MPST coordination observer tools (ADR-009)
                "csm_list_protocols"      => csm_list_protocols(CsmListProtocolsParams),
                "csm_protocol_of_pattern" => csm_protocol_of_pattern(CsmProtocolOfPatternParams),
                "csm_show_projection"     => csm_show_projection(CsmShowProjectionParams),
                "csm_validate_run"        => csm_validate_run(CsmValidateRunParams),
                "csm_protocol_plan"       => csm_protocol_plan(CsmProtocolPlanParams),
                "csm_infer_peer_fsm"      => csm_infer_peer_fsm(CsmInferPeerFsmParams),
                "a2a_report_outcome"     => a2a_report_outcome(A2aReportOutcomeParams),
                // Scientific-experiment subsystem (share the tool_experiments module).
                "experiment_open"               => experiment_open(ExperimentOpenParams) in tool_experiments,
                "experiment_protocol"           => experiment_protocol(ExperimentProtocolParams) in tool_experiments,
                "experiment_record_measurement" => experiment_record_measurement(ExperimentRecordMeasurementParams) in tool_experiments,
                "experiment_decide"             => experiment_decide(ExperimentDecideParams) in tool_experiments,
                "experiment_search"             => experiment_search(ExperimentSearchParams) in tool_experiments,
                "experiment_get"                => experiment_get(ExperimentGetParams) in tool_experiments,
                "experiment_list"               => experiment_list(ExperimentListParams) in tool_experiments,
                "experiment_timeline"           => experiment_timeline(ExperimentTimelineParams) in tool_experiments,
                "experiment_log_artifact"       => experiment_log_artifact(ExperimentLogArtifactParams) in tool_experiments,
                "experiment_render_ledger"      => experiment_render_ledger(ExperimentRenderLedgerParams) in tool_experiments,
                // Work-item / plan tracker subsystem (share the work_items module).
                "work_item_create"       => work_item_create(WorkItemCreateParams) in work_items,
                "work_item_get"          => work_item_get(WorkItemGetParams) in work_items,
                "work_item_update"       => work_item_update(WorkItemUpdateParams) in work_items,
                "work_item_list"         => work_item_list(WorkItemListParams) in work_items,
                "work_item_tree"         => work_item_tree(WorkItemTreeParams) in work_items,
                "work_item_reparent"     => work_item_reparent(WorkItemReparentParams) in work_items,
                "work_item_set_status"   => work_item_set_status(WorkItemSetStatusParams) in work_items,
                // Work-item tracker Phase 2 — tags + progress (share the work_items module).
                "tag_create"             => tag_create(TagCreateParams) in work_items,
                "tag_list"               => tag_list(TagListParams) in work_items,
                "tag_merge"              => tag_merge(TagMergeParams) in work_items,
                "tag_rename"             => tag_rename(TagRenameParams) in work_items,
                "work_item_tag"          => work_item_tag(WorkItemTagParams) in work_items,
                "work_item_untag"        => work_item_untag(WorkItemUntagParams) in work_items,
                "work_item_record_progress" => work_item_record_progress(WorkItemRecordProgressParams) in work_items,
                "work_item_progress_log" => work_item_progress_log(WorkItemProgressLogParams) in work_items,
                "work_item_completion"   => work_item_completion(WorkItemCompletionParams) in work_items,
                "work_item_reprioritize" => work_item_reprioritize(WorkItemReprioritizeParams) in work_items,
                "work_item_search"       => work_item_search(WorkItemSearchParams) in work_items,
                "plan_define"            => plan_define(PlanDefineParams) in work_items,
                "plan_validate"          => plan_validate(PlanValidateParams) in work_items,
                "plan_definition_export" => plan_definition_export(PlanDefinitionExportParams) in work_items,
                "plan_definition_import" => plan_definition_import(PlanDefinitionImportParams) in work_items,
                "work_item_add_criterion" => work_item_add_criterion(WorkItemAddCriterionParams) in work_items,
                "work_item_record_evidence" => work_item_record_evidence(WorkItemRecordEvidenceParams) in work_items,
                "work_item_attempt_verify" => work_item_attempt_verify(WorkItemAttemptVerifyParams) in work_items,
                "work_item_defer"        => work_item_defer(WorkItemDeferParams) in work_items,
                "work_item_reinstate"    => work_item_reinstate(WorkItemReinstateParams) in work_items,
                "work_item_ingest_plan"  => work_item_ingest_plan(WorkItemIngestPlanParams) in work_items,
                "work_item_promote_marker" => work_item_promote_marker(WorkItemPromoteMarkerParams) in work_items,
                "work_item_claim"        => work_item_claim(WorkItemClaimParams) in work_items,
                "work_item_claim_next"   => work_item_claim_next(WorkItemClaimNextParams) in work_items,
                "work_item_release"      => work_item_release(WorkItemReleaseParams) in work_items,
                "work_item_handoff"      => work_item_handoff(WorkItemHandoffParams) in work_items,
                "agent_heartbeat"        => agent_heartbeat(AgentHeartbeatParams) in work_items,
                "work_item_who_owns"     => work_item_who_owns(WorkItemWhoOwnsParams) in work_items,
                "agent_activity"         => agent_activity(AgentActivityParams) in work_items,
                "work_item_activity"     => work_item_activity(WorkItemActivityParams) in work_items,
                "work_item_link"         => work_item_link(WorkItemLinkParams) in work_items,
                "work_item_unlink"       => work_item_unlink(WorkItemUnlinkParams) in work_items,
                "work_item_cycles"       => work_item_cycles(WorkItemCyclesParams) in work_items,
                "work_item_anchor_code"  => work_item_anchor_code(WorkItemAnchorCodeParams) in work_items,
                "work_item_burndown"     => work_item_burndown(WorkItemBurndownParams) in work_items,
                "work_item_export"       => work_item_export(WorkItemExportParams) in work_items,
                "work_item_link_experiment" => work_item_link_experiment(WorkItemLinkExperimentParams) in work_items,
                "a2a_pattern_recursive"  => a2a_pattern_recursive(A2aPatternRecursiveParams),
                "trajectory_similarity"  => trajectory_similarity(TrajectorySimilarityParams),
                "recognize_trajectory"   => recognize_trajectory(RecognizeTrajectoryParams) in tool_trajectory_similarity,
                // SOTA Phase 2 — graph algorithms
                "kcore_analysis"         => kcore_analysis(KcoreAnalysisParams),
                "ktruss_analysis"        => ktruss_analysis(KtrussAnalysisParams),
                "personalized_pagerank"  => personalized_pagerank(PersonalizedPagerankParams),
                "edge_betweenness"       => edge_betweenness(EdgeBetweennessParams),
                "structural_holes"       => structural_holes(StructuralHolesParams),
                "motif_census"           => motif_census(MotifCensusParams),
                "attack_vulnerability"   => attack_vulnerability(AttackVulnerabilityParams),
                // SOTA Phase 3 — information theory
                "compression_distance"   => compression_distance(CompressionDistanceParams),
                "cochange_mutual_information" => cochange_mutual_information(CochangeMutualInformationParams),
                "import_entropy"         => import_entropy(ImportEntropyParams),
                "identifier_entropy"     => identifier_entropy(IdentifierEntropyParams),
                // SOTA Phase 4 — evolution + quality
                "bus_factor"             => bus_factor(BusFactorParams),
                "knowledge_silos"        => knowledge_silos(KnowledgeSilosParams),
                "ownership_coupling_mismatch" => ownership_coupling_mismatch(OwnershipCouplingMismatchParams),
                "doc_code_drift"         => doc_code_drift(DocCodeDriftParams),
                "test_smells"            => test_smells(TestSmellsParams),
                "mutation_score_surrogate" => mutation_score_surrogate(MutationScoreSurrogateParams),
                "flaky_test_candidates"  => flaky_test_candidates(FlakyTestCandidatesParams),
                // SOTA Phase 5 — concurrency / safety / performance
                "lockset_races"          => lockset_races(LocksetRacesParams),
                "unsafe_clusters"        => unsafe_clusters(UnsafeClustersParams),
                "panic_paths"            => panic_paths(PanicPathsParams),
                "deadlock_candidates"    => deadlock_candidates(DeadlockCandidatesParams),
                "send_sync_violations"   => send_sync_violations(SendSyncViolationsParams),
                "quadratic_loops"        => quadratic_loops(QuadraticLoopsParams),
                "missing_preallocation"  => missing_preallocation(MissingPreallocationParams),
                "blocking_in_async"      => blocking_in_async(BlockingInAsyncParams),
                "clone_density"          => clone_density(CloneDensityParams),
                "io_hotpath"             => io_hotpath(IoHotpathParams),
                // SOTA Phase 6 — security
                "taint_analysis"         => taint_analysis(TaintAnalysisParams),
                "secret_detection"       => secret_detection(SecretDetectionParams),
                "crypto_misuse"          => crypto_misuse(CryptoMisuseParams),
                "unsafe_deserialization" => unsafe_deserialization(UnsafeDeserializationParams),
                "injection_candidates"   => injection_candidates(InjectionCandidatesParams),
                "unprotected_routes"     => unprotected_routes(UnprotectedRoutesParams),
                "cve_supply_chain"       => cve_supply_chain(CveSupplyChainParams),
                // SOTA Phase 7 — API / contract
                "public_api_surface"     => public_api_surface(PublicApiSurfaceParams),
                "semver_break_audit"     => semver_break_audit(SemverBreakAuditParams),
                "deprecated_but_used"    => deprecated_but_used(DeprecatedButUsedParams),
                "api_stability"          => api_stability(ApiStabilityParams),
                // SOTA Phase 8 — ML / embedding-based
                "lsh_clone_detection"    => lsh_clone_detection(LshCloneDetectionParams),
                "semantic_drift"         => semantic_drift(SemanticDriftParams),
                "embedding_outliers"     => embedding_outliers(EmbeddingOutliersParams),
                "multi_resolution_pagerank" => multi_resolution_pagerank(MultiResolutionPagerankParams),
                // SOTA Phase 9 — data engineering
                "migration_safety"       => migration_safety(MigrationSafetyParams),
                "dead_columns"           => dead_columns(DeadColumnsParams),
                "pii_spread"             => pii_spread(PiiSpreadParams),
                // SOTA Phase 10 — call-graph downstream
                "dead_code_reachability" => dead_code_reachability(DeadCodeReachabilityParams),
                "feature_envy"           => feature_envy(FeatureEnvyParams),
                "shotgun_surgery"        => shotgun_surgery(ShotgunSurgeryParams),
                "lcom4"                  => lcom4(Lcom4Params),
                // SOTA Phase 11 — evolution analytics
                "refactor_pressure"      => refactor_pressure(RefactorPressureParams),
                "commit_changepoint"     => commit_changepoint(CommitChangepointParams),
                "commit_topic_drift"     => commit_topic_drift(CommitTopicDriftParams),
                "release_api_stability"  => release_api_stability(ReleaseApiStabilityParams),
                // Advanced
                "code_summarize"         => code_summarize(CodeSummarizeParams),
                "engineering_scorecard"  => engineering_scorecard(EngineeringScorecardParams),
                // Telemetry
                "mcp_tool_telemetry"     => mcp_tool_telemetry(McpToolTelemetryParams),
                // Orientation / multi-axis tools previously omitted from the
                // dispatch — added so `call_tool_cli` can drive every #[tool]
                // method from harness tests. See `query_smoke_mcp_tools.rs`.
                "orient"                         => orient(OrientParams),
                "topic_hierarchy_fcm"            => topic_hierarchy_fcm(TopicHierarchyFcmParams),
                "dependency_health"              => dependency_health(DependencyHealthParams),
                "shotgun_surgery_fix"            => shotgun_surgery_fix(ShotgunSurgeryFixParams),
                "pr_scope_recommender"           => pr_scope(PrScopeRecommenderParams) in tool_pr_scope,
                "naming_consistency"             => naming_consistency(NamingConsistencyParams),
                "adoption_lag"                   => adoption_lag(AdoptionLagParams),
                "merge_conflict_risk"            => merge_conflict_risk(MergeConflictRiskParams),
                "hot_path_audit"                 => hot_path_audit(HotPathAuditParams),
                "bus_factor_map"                 => bus_factor_map(BusFactorMapParams),
                "module_growth_trajectory"       => module_growth(ModuleGrowthParams) in tool_module_growth,
                "stale_zombie_detector"          => stale_zombie(StaleZombieParams) in tool_stale_zombie,
                "tech_debt_burn_down"            => tech_debt_burn_down(TechDebtBurnDownParams),
                "internal_dry"                   => internal_dry(InternalDryParams),
                "extraction_candidates"          => extraction_candidates(ExtractionCandidatesParams),
                "boilerplate_clusters"           => boilerplate_clusters(BoilerplateClustersParams),
                "chunk_clusters"                 => chunk_clusters(ChunkClustersParams),
                "pattern_abstraction_candidates" => pattern_abstraction(PatternAbstractionParams) in tool_pattern_abstraction,
                "pattern_search"                 => pattern_search(PatternSearchParams),
                "recommend_layering"             => recommend_layering(RecommendLayeringParams),
                "recommend_module_split"         => recommend_module_split(RecommendModuleSplitParams),
                "reviewer_recommender"           => reviewer_recommender(ReviewerRecommenderParams),
                "fix_circular_dependency"        => fix_circular_dependency(FixCircularDependencyParams),
            }, no_params: {
                "list_projects" => list_projects,
                "index_stats"   => index_stats,
                "reindex"       => reindex,
                "pattern_catalog_stats" => pattern_catalog_stats in tool_software_patterns,
            })
        };
        instrumented_tool_run(self.stats(), name, None, caller, &params_summary, None, fut).await
    }
}

#[tool_handler]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_completions()
                .enable_logging()
                .enable_tasks()
                .build(),
        )
        .with_server_info(Implementation::new("pgmcp", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "pgmcp indexes the user's development workspaces into PostgreSQL+pgvector and \
             exposes ~72 tools for cross-project search, semantic queries, graph analysis, \
             code-health metrics, and recommendation-shaped refactoring actions.\n\n\
             USE THESE TOOLS BEFORE built-in Read/Grep/Glob when the question is conceptual \
             ('how does X work?'), cross-project ('does this pattern exist elsewhere?'), \
             graph-shaped ('what depends on this?'), or about code health ('where is the \
             technical debt?'). Built-in tools remain right for narrow within-cwd operations \
             and for files just written this turn (not yet in the index).\n\n\
             FIRST STEP for unfamiliar codebases or non-trivial tasks: call `orient` — it \
             bundles project_tree, key entry points by PageRank, recently-changed files, \
             top topics, mandate sources, and a `health` envelope into one call so you don't \
             have to scatter across half a dozen tools to get oriented. Use `mandate_context` \
             when you specifically need the effective AGENTS.md/CLAUDE.md/.pgmcp.toml bundle \
             for a project or cwd.\n\n\
             The 'claude' project indexes ~/.claude/ — past Claude Code sessions, memory \
             files, plans. Use semantic_search or text_search with project: \"claude\" to \
             retrieve prior context, decisions, and plans.\n\n\
             ### Tool catalog\n\n\
             SEARCH: orient (composite first-step), semantic_search (vector similarity, \
             conceptual queries), text_search (Postgres full-text, exact keywords), \
             grep (regex across all indexed files), hybrid_search (BM25+vector RRF — best \
             for queries that benefit from both keyword and concept), search_commits (git \
             history semantic search; requires [git] index_history = true).\n\n\
             READ/INVENTORY: read_file, file_info, list_projects, project_tree, \
             mandate_context, index_stats.\n\n\
             CROSS-PROJECT SIMILARITY: compare_files (real-time chunk-level), \
             find_similar_modules (materialized table), find_duplicates (union-find \
             clusters), refactoring_report (actionable extraction candidates).\n\n\
             TOPIC DISCOVERY (Fuzzy BERTopic = FCM + c-TF-IDF): discover_topics, \
             topic_hierarchy, topic_hierarchy_fcm — soft-clustering chunks into \
             keyword-labeled topics. With project param = real-time intra-project; \
             without = cached cross-project.\n\n\
             CODE ANALYSIS: find_orphans (low topic membership), find_misplaced_code \
             (semantic vs directory mismatch), find_coupled_files (git co-change Jaccard), \
             test_coverage_gaps, complexity_hotspots, doc_coverage_gaps, suggest_merges, \
             suggest_splits.\n\n\
             GRAPH: dependency_graph (DOT/edges/summary), centrality_analysis (PageRank, \
             betweenness, degree), community_detection (Louvain), circular_dependencies \
             (Tarjan SCC), change_impact_analysis (graph + co-change + semantic).\n\n\
             ARCHITECTURE & DESIGN: coupling_cohesion_report (Robert C. Martin Ca/Ce/I/A/D*), \
             architecture_violations, design_smell_detection (god class, SRP violation, \
             shotgun surgery, stale module, unstable dependency), architecture_quality \
             (10-dim 0-100% scorecard), design_metrics (cyclomatic, WMC, maintainability).\n\n\
             PREDICTION: bug_prediction (churn × complexity × fix ratio), \
             technical_debt_analysis (TODO density + complexity + test gaps + churn), \
             anomaly_detection (embedding distance from project centroid).\n\n\
             SUMMARIZATION & SCORECARD: code_summarize (structural roll-up), \
             engineering_scorecard (10-dim A-F + GPA + ORR checklist).\n\n\
             REFACTORING & RECOMMENDATIONS (every finding embeds a typed `recommended_fix` \
             with action + steps + confidence + estimated_effort): chunk_clusters \
             (chunk-level cross-project DRY), internal_dry (within-file DRY), \
             extraction_candidates (ranked extract-to-shared-crate; superset of \
             refactoring_report), pattern_abstraction_candidates (medium-similarity → \
             trait/interface/protocol), boilerplate_clusters (codegen-worthy near-identical \
             chunks → macros/generics), recommend_module_split (god-file → multiple files \
             via FCM topic grouping), recommend_layering (Louvain + SDP layered \
             architecture proposal), shotgun_surgery_fix (consolidate scattered \
             hub-and-spoke logic), fix_circular_dependency (per-cycle edge-break \
             recommendation via PageRank-delta), stale_zombie_detector (low PageRank + low \
             in-degree + author abandonment → delete or move), naming_consistency \
             (per-(directory, kind) convention divergence; requires file_symbols), \
             tech_debt_burn_down (capstone — phased remediation plan from all findings, \
             cost-benefit ranked, packed into now/next/later phases).\n\n\
             PR & TEAM WORKFLOW: pr_scope_recommender (min/recommended/max PR scope from a \
             starter file), hot_path_audit (PageRank ∩ churn ∩ fix-ratio intersection with \
             P0/P1/P2 priority), bus_factor_map (knowledge-concentration risk per file via \
             blame), reviewer_recommender (rank reviewers by recent file ownership), \
             merge_conflict_risk (peer-overlap on a branch's files; window-based), \
             dependency_health (external/unresolved-import audit; \
             prune/upgrade/consolidate/keep), pattern_search (embed snippet, find matches \
             across projects, verdict reuse|adapt|new with recommended_fix).\n\n\
             TRAJECTORY & ADOPTION: module_growth_trajectory (LOC + chunks + churn over \
             time, predicts god_module emergence via linear regression), adoption_lag (find \
             legacy usages of a modern reference file via kNN + age filter, recommends \
             merge_files / move_function).",
        )
    }

    // ── Lifecycle ────────────────────────────────────────────────────────

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        tracing::info!("Client initialized, registering peer for log broadcasting");
        self.log_broadcaster().add_peer(context.peer.clone());
    }

    // ── Completions ──────────────────────────────────────────────────────

    async fn complete(
        &self,
        request: CompleteRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, McpError> {
        super::completions::handle_complete(self.db().as_ref(), request).await
    }

    // ── Logging ──────────────────────────────────────────────────────────

    async fn set_level(
        &self,
        request: SetLevelRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        tracing::info!(level = ?request.level, "Client set logging level");
        self.log_broadcaster().set_level(request.level);
        Ok(())
    }

    // ── Resources ────────────────────────────────────────────────────────

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult {
            resources: vec![
                RawResource::new("pgmcp://stats", "Indexing Statistics")
                    .with_description("Current indexing statistics (JSON)")
                    .no_annotation(),
                RawResource::new("pgmcp://projects", "Indexed Projects")
                    .with_description("List of indexed projects (JSON)")
                    .no_annotation(),
                RawResource::new("pgmcp://workspace/mandates", "Workspace Mandates")
                    .with_description(
                        "Effective workspace-level AGENTS.md/CLAUDE.md mandate sources (JSON)",
                    )
                    .no_annotation(),
            ],
            next_cursor: None,
            meta: None,
        })
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult {
            resource_templates: vec![
                RawResourceTemplate::new("pgmcp://project/{name}", "Project Info")
                    .with_description("Project details by name")
                    .no_annotation(),
                RawResourceTemplate::new("pgmcp://project/{name}/tree", "Project Tree")
                    .with_description("File tree for a project")
                    .no_annotation(),
                RawResourceTemplate::new("pgmcp://project/{name}/mandates", "Project Mandates")
                    .with_description("Effective mandate bundle for a project")
                    .no_annotation(),
                RawResourceTemplate::new("pgmcp://file/{path}", "File Content")
                    .with_description("Read an indexed file by relative path")
                    .no_annotation(),
            ],
            next_cursor: None,
            meta: None,
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let uri: &str = &request.uri;

        // Static resources
        match uri {
            "pgmcp://stats" => {
                let snapshot = self.stats().snapshot();
                let json = serde_json::to_string_pretty(&snapshot)
                    .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                    json,
                    request.uri.clone(),
                )]));
            }
            "pgmcp://projects" => {
                let projects = self
                    .db()
                    .list_projects()
                    .await
                    .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                let json = serde_json::to_string_pretty(&projects)
                    .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                    json,
                    request.uri.clone(),
                )]));
            }
            "pgmcp://workspace/mandates" => {
                let config = self.config().load();
                let bundle = crate::mandates::resolve_effective_mandates(&config, None);
                let json = serde_json::to_string_pretty(&bundle)
                    .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                    json,
                    request.uri.clone(),
                )]));
            }
            _ => {}
        }

        // Templated resources
        if let Some(rest) = uri.strip_prefix("pgmcp://project/") {
            if let Some(name) = rest.strip_suffix("/mandates") {
                // pgmcp://project/{name}/mandates
                let projects = self
                    .db()
                    .list_projects()
                    .await
                    .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                let project = projects.into_iter().find(|p| p.name == name);
                match project {
                    Some(p) => {
                        let config = self.config().load();
                        let bundle = crate::mandates::resolve_effective_mandates(&config, Some(&p));
                        let json = serde_json::to_string_pretty(&bundle)
                            .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                        return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                            json,
                            request.uri.clone(),
                        )]));
                    }
                    None => {
                        return Err(McpError::resource_not_found(
                            format!("Project not found: {}", name),
                            None,
                        ));
                    }
                }
            }
            if let Some(name) = rest.strip_suffix("/tree") {
                // pgmcp://project/{name}/tree
                let paths = self
                    .db()
                    .project_tree(name, 10)
                    .await
                    .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                let tree = paths.join("\n");
                return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                    tree,
                    request.uri.clone(),
                )]));
            }
            // pgmcp://project/{name}
            let name = rest;
            let projects = self
                .db()
                .list_projects()
                .await
                .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
            let project = projects.into_iter().find(|p| p.name == name);
            match project {
                Some(p) => {
                    let json = serde_json::to_string_pretty(&p)
                        .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                    return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                        json,
                        request.uri.clone(),
                    )]));
                }
                None => {
                    return Err(McpError::resource_not_found(
                        format!("Project not found: {}", name),
                        None,
                    ));
                }
            }
        }

        if let Some(path) = uri.strip_prefix("pgmcp://file/") {
            // pgmcp://file/{path} — search by relative_path
            let file = self
                .db()
                .read_file_by_relative_path(path)
                .await
                .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
            match file {
                Some(f) => {
                    let json = serde_json::to_string_pretty(&f)
                        .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                    return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                        json,
                        request.uri.clone(),
                    )]));
                }
                None => {
                    return Err(McpError::resource_not_found(
                        format!("File not found: {}", path),
                        None,
                    ));
                }
            }
        }

        Err(McpError::resource_not_found(
            format!("Unknown resource: {}", uri),
            None,
        ))
    }

    // ── Tasks ────────────────────────────────────────────────────────────

    async fn enqueue_task(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CreateTaskResult, McpError> {
        match &*request.name {
            "reindex" => {
                let (task_id, cancel_flag) = self.task_store().create_task("reindex");
                let task = self
                    .task_store()
                    .get_task(&task_id)
                    .expect("Task was just created");

                // The reindex task spawns its own future; we need an owned
                // PgPool to pass into the spawn. Until reindex itself moves
                // to a trait method, clone the pool out via the escape hatch.
                let db_pool = self.pool().clone();
                let task_store = Arc::clone(self.task_store());
                let log_broadcaster = Arc::clone(self.log_broadcaster());

                tokio::spawn(async move {
                    task_store.update_progress(&task_id, "Clearing file chunks...");
                    log_broadcaster.log(
                        LoggingLevel::Info,
                        "pgmcp::reindex",
                        serde_json::json!({"message": "Reindex task started, clearing chunks"}),
                    );

                    if cancel_flag.load(Ordering::Acquire) {
                        return;
                    }

                    if let Err(e) = sqlx::query("DELETE FROM file_chunks")
                        .execute(&db_pool)
                        .await
                    {
                        task_store.fail_task(&task_id, &format!("Failed to clear chunks: {}", e));
                        return;
                    }

                    task_store.update_progress(&task_id, "Clearing indexed files...");

                    if cancel_flag.load(Ordering::Acquire) {
                        return;
                    }

                    if let Err(e) = sqlx::query("DELETE FROM indexed_files")
                        .execute(&db_pool)
                        .await
                    {
                        task_store.fail_task(&task_id, &format!("Failed to clear files: {}", e));
                        return;
                    }

                    log_broadcaster.log(
                        LoggingLevel::Info,
                        "pgmcp::reindex",
                        serde_json::json!({"message": "Index cleared, background scanner will re-index"}),
                    );

                    task_store.complete_task(
                        &task_id,
                        serde_json::json!({
                            "message": "Index cleared. Files will be re-indexed automatically by the background scanner."
                        }),
                    );
                });

                Ok(CreateTaskResult::new(task))
            }
            other => Err(McpError::internal_error(
                format!("Task processing not supported for tool: {}", other),
                None,
            )),
        }
    }

    async fn list_tasks(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListTasksResult, McpError> {
        Ok(ListTasksResult::new(self.task_store().list_tasks()))
    }

    async fn get_task_info(
        &self,
        request: GetTaskInfoParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        match self.task_store().get_task(&request.task_id) {
            Some(task) => Ok(GetTaskResult { meta: None, task }),
            None => Err(McpError::internal_error(
                format!("Task not found: {}", request.task_id),
                None,
            )),
        }
    }

    async fn get_task_result(
        &self,
        request: GetTaskResultParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskPayloadResult, McpError> {
        match self.task_store().get_result(&request.task_id) {
            Some(result) => Ok(GetTaskPayloadResult::new(result)),
            None => {
                // Check if task exists but has no result yet
                if self.task_store().get_task(&request.task_id).is_some() {
                    Err(McpError::internal_error("Task is still in progress", None))
                } else {
                    Err(McpError::internal_error(
                        format!("Task not found: {}", request.task_id),
                        None,
                    ))
                }
            }
        }
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CancelTaskResult, McpError> {
        match self.task_store().cancel_task(&request.task_id) {
            Some(task) => Ok(CancelTaskResult { meta: None, task }),
            None => Err(McpError::internal_error(
                format!("Task not found: {}", request.task_id),
                None,
            )),
        }
    }
}

// Cross-crate tool unit tests live under `pgmcp-testing/tests/` to avoid
// Cargo's cyclic-dev-dep limitation (pgmcp ↔ pgmcp-testing). See the
// note in `Cargo.toml`'s `[dev-dependencies]` block.
