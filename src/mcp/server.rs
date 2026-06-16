//! MCP Server implementation using rmcp.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use arc_swap::ArcSwap;

#[path = "server/error_classify.rs"]
mod error_classify;

#[path = "server/support.rs"]
mod support;
pub(crate) use support::*;

// Tool parameter types live in their own per-domain files under
// `src/mcp/params/`. Re-exported here so `crate::mcp::server::<Name>Params`
// resolves unchanged for every tool body file and the dispatch macros.
#[path = "params/mod.rs"]
pub mod params;
pub use params::*;

// Per-domain `#[tool]` handler impl blocks. Each file holds a
// `#[tool_router(router = router_<domain>)]` impl McpServer block; the
// composed router is assembled in `assembled_tool_router()` below. These
// modules only *add* methods/routers to `McpServer`; nothing is re-exported.
#[path = "server/handlers/a2a.rs"]
mod handlers_a2a;
#[path = "server/handlers/api_contract.rs"]
mod handlers_api_contract;
#[path = "server/handlers/architecture.rs"]
mod handlers_architecture;
#[path = "server/handlers/callgraph_evo.rs"]
mod handlers_callgraph_evo;
#[path = "server/handlers/concurrency.rs"]
mod handlers_concurrency;
#[path = "server/handlers/core.rs"]
mod handlers_core;
#[path = "server/handlers/core_advanced.rs"]
mod handlers_core_advanced;
#[path = "server/handlers/csm.rs"]
mod handlers_csm;
#[path = "server/handlers/data_eng.rs"]
mod handlers_data_eng;
#[path = "server/handlers/data_tables.rs"]
mod handlers_data_tables;
#[path = "server/handlers/experiments.rs"]
mod handlers_experiments;
#[path = "server/handlers/fuzzy.rs"]
mod handlers_fuzzy;
#[path = "server/handlers/graph_advanced.rs"]
mod handlers_graph_advanced;
#[path = "server/handlers/graph_core.rs"]
mod handlers_graph_core;
#[path = "server/handlers/graph_func.rs"]
mod handlers_graph_func;
#[path = "server/handlers/infotheory.rs"]
mod handlers_infotheory;
#[path = "server/handlers/inventory.rs"]
mod handlers_inventory;
#[path = "server/handlers/memory_crud.rs"]
mod handlers_memory_crud;
#[path = "server/handlers/memory_search.rs"]
mod handlers_memory_search;
#[path = "server/handlers/meta.rs"]
mod handlers_meta;
#[path = "server/handlers/ml_embedding.rs"]
mod handlers_ml_embedding;
#[path = "server/handlers/ontology.rs"]
mod handlers_ontology;
#[path = "server/handlers/patterns.rs"]
mod handlers_patterns;
#[path = "server/handlers/prediction.rs"]
mod handlers_prediction;
#[path = "server/handlers/quality_evo.rs"]
mod handlers_quality_evo;
#[path = "server/handlers/recommend.rs"]
mod handlers_recommend;
#[path = "server/handlers/security.rs"]
mod handlers_security;
#[path = "server/handlers/sema.rs"]
mod handlers_sema;
#[path = "server/handlers/similarity.rs"]
mod handlers_similarity;
#[path = "server/handlers/toolbox.rs"]
mod handlers_toolbox;
#[path = "server/handlers/topics.rs"]
mod handlers_topics;
#[path = "server/handlers/trajectory.rs"]
mod handlers_trajectory;
#[path = "server/handlers/work_items_a.rs"]
mod handlers_work_items_a;
#[path = "server/handlers/work_items_b.rs"]
mod handlers_work_items_b;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::*;
use rmcp::service::{NotificationContext, RequestContext};
use rmcp::tool;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
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

/// Extract the MCP session id from the streamable-HTTP transport. rmcp's
/// tower layer injects the originating `http::request::Parts` into the
/// request `extensions`; the `mcp-session-id` header lives there. Returns
/// `None` for transports without a session (the stdio debug path) and for
/// CLI dispatch (`call_tool_cli`), which has no `RequestContext` at all.
/// Both target clients (Claude Code, Codex) connect over HTTP, so this is
/// populated for real tool calls and lets adoption telemetry aggregate per
/// session rather than only per client.
pub(crate) fn extract_mcp_session_id(ctx: &RequestContext<RoleServer>) -> Option<String> {
    let parts = ctx.extensions.get::<axum::http::request::Parts>()?;
    parts
        .headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Extract the connecting client's TCP peer address (source ip:port) from the
/// streamable-HTTP transport. The daemon serves the router via
/// `into_make_service_with_connect_info::<SocketAddr>()` (`src/cli/daemon.rs`),
/// so axum inserts `ConnectInfo<SocketAddr>` into the request extensions, and
/// rmcp's tower layer forwards the whole `http::request::Parts` (including its
/// `.extensions`) into `ctx.extensions` — the same seam `extract_mcp_session_id`
/// uses for headers. Returns `None` for the stdio debug path and CLI dispatch
/// (no `RequestContext` / no ConnectInfo). The peer port is the key that
/// `crate::proc_clients::resolve_pid_for_peer` maps back to the client PID via
/// `/proc/net/tcp`. If a future rmcp build drops `Parts.extensions`, fall back
/// to a tower `from_fn` layer that stamps the port into an `x-pgmcp-peer-port`
/// header (headers are known to survive — see `extract_mcp_session_id`).
pub(crate) fn extract_peer_addr(ctx: &RequestContext<RoleServer>) -> Option<std::net::SocketAddr> {
    let parts = ctx.extensions.get::<axum::http::request::Parts>()?;
    parts
        .extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0)
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
    instrumented_tool_wrap_with_project(stats, name, secs, ctx, params_summary, None, fut).await
}

/// Same as `instrumented_tool_wrap`, but allows handlers whose parameter type
/// has a `project` field to persist that project in durable telemetry.
pub(crate) async fn instrumented_tool_wrap_with_project<F>(
    stats: &StatsTracker,
    name: &str,
    secs: u64,
    ctx: &RequestContext<RoleServer>,
    params_summary: &str,
    project_hint: Option<String>,
    fut: F,
) -> Result<CallToolResult, McpError>
where
    F: std::future::Future<Output = Result<CallToolResult, McpError>>,
{
    let caller = extract_caller(ctx);
    let request_id = Some(format!("{:?}", ctx.id));
    let mcp_session_id = extract_mcp_session_id(ctx);
    // Capture the client's OS identity (PID/cwd/project) once per session. Only
    // the HTTP transport carries both a session id and a TCP peer; stdio/CLI
    // yield `None` and skip. The ~100 ms /proc resolution + DB upsert happen off
    // this hot path in the client-writer task (`note_client` is O(1) here).
    if let (Some(sid), Some(peer)) = (mcp_session_id.as_deref(), extract_peer_addr(ctx)) {
        stats.note_client(
            sid,
            &caller.client_name,
            &caller.client_version,
            &caller.protocol_version,
            peer,
        );
    }
    instrumented_tool_run(
        stats,
        name,
        Some(secs),
        caller,
        params_summary,
        request_id,
        mcp_session_id,
        normalize_telemetry_string(project_hint.as_deref()),
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
    mcp_session_id: Option<String>,
    project_hint: Option<String>,
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
    // Measure the serialized result size (success only) for the `output_bytes`
    // telemetry: sum of text-content byte lengths plus any structured content.
    // O(result length), on the async telemetry path — no hot-path cost.
    let result_bytes: Option<i32> = result.as_ref().ok().map(|r| {
        let mut total: usize = r
            .content
            .iter()
            .filter_map(|c| c.as_text())
            .map(|t| t.text.len())
            .sum();
        if let Some(sc) = &r.structured_content {
            total += sc.to_string().len();
        }
        total.min(i32::MAX as usize) as i32
    });
    let result_tokens_est = result_bytes.map(|b| (b + 3) / 4);
    let row = crate::stats::telemetry_writer::TelemetryRow {
        tool: name.to_string(),
        client_name: caller.client_name.clone(),
        client_version: Some(caller.client_version.clone()),
        protocol_version: Some(caller.protocol_version.clone()),
        mcp_session_id,
        project: project_hint,
        cwd: None,
        duration_ms: (duration_ns / 1_000_000).min(i32::MAX as u64) as i32,
        outcome,
        error_class,
        request_id,
        params_sha256,
        result_bytes,
        result_tokens_est,
    };
    crate::stats::telemetry_writer::try_enqueue(stats, row);
    result
}

/// Re-encode a tool result's JSON text content into the caller's preferred
/// wire format. Applied centrally at the `call_tool` dispatch boundary so EVERY
/// tool — the ~88 that serialize through `sota_helpers::json_result`, the handful
/// with their own `json_result`, and the ~87 that inline `to_string_pretty` —
/// honors a token-sensitive client's compact format without threading a
/// parameter through 300+ tool signatures or editing every body.
///
/// Only `CompactJson` callers (e.g. codex) trigger work: each text block that is
/// pretty-printed JSON (cheap `'\n'` guard) is parsed and re-serialized compact,
/// trimming the ~30-40% whitespace overhead. Non-JSON text (markdown/org reports
/// from `src/render`, plain prose) fails the parse and is left untouched; the
/// default `Markdown` posture (claude-code) is a no-op, so rich clients keep
/// byte-identical pretty output. Idempotent: already-compact JSON has no newline
/// and is skipped.
pub(crate) fn reencode_result_for_format(
    mut result: CallToolResult,
    rc: crate::mcp::client_profile::RenderCtx,
) -> CallToolResult {
    use crate::mcp::client_profile::OutputFormat;
    if rc.output_format != OutputFormat::CompactJson {
        return result;
    }
    for content in result.content.iter_mut() {
        let compact = content.as_text().and_then(|t| {
            if t.text.contains('\n') {
                serde_json::from_str::<serde_json::Value>(&t.text)
                    .ok()
                    .map(|v| v.to_string())
            } else {
                None
            }
        });
        if let Some(compact) = compact {
            *content = Content::text(compact);
        }
    }
    result
}

fn normalize_telemetry_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn telemetry_project_from_json(args: &serde_json::Value) -> Option<String> {
    normalize_telemetry_string(args.get("project").and_then(serde_json::Value::as_str))
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

    /// Compose the full tool router from the per-domain `router_*` builders.
    /// `ToolRouter<S>` implements `Add`, so the per-block routers (each
    /// generated by a `#[tool_router(router = router_<domain>)]` impl in
    /// `src/mcp/server/handlers/`) sum into one. Every `#[tool]` keeps its
    /// original name, so the exposed tool set is byte-identical to the
    /// pre-split single-block router.
    fn assembled_tool_router() -> ToolRouter<Self> {
        Self::router_core()
            + Self::router_memory_crud()
            + Self::router_memory_search()
            + Self::router_inventory()
            + Self::router_similarity()
            + Self::router_recommend()
            + Self::router_patterns()
            + Self::router_topics()
            + Self::router_graph_core()
            + Self::router_architecture()
            + Self::router_prediction()
            + Self::router_a2a()
            + Self::router_csm()
            + Self::router_experiments()
            + Self::router_work_items_a()
            + Self::router_work_items_b()
            + Self::router_data_tables()
            + Self::router_trajectory()
            + Self::router_graph_advanced()
            + Self::router_infotheory()
            + Self::router_quality_evo()
            + Self::router_graph_func()
            + Self::router_concurrency()
            + Self::router_security()
            + Self::router_api_contract()
            + Self::router_ml_embedding()
            + Self::router_data_eng()
            + Self::router_callgraph_evo()
            + Self::router_core_advanced()
            + Self::router_sema()
            + Self::router_fuzzy()
            + Self::router_ontology()
            + Self::router_toolbox()
            + Self::router_meta()
    }
}

// Construction + router assembly. No `#[tool_router]` here: every `#[tool]`
// method now lives in a per-domain `impl McpServer` block under
// `src/mcp/server/handlers/`, each with its own `router_<domain>`. This block
// holds only the constructor, the static catalog accessor, and the `pool`
// escape hatch.
impl McpServer {
    /// Create a new MCP server from a `SystemContext` bundle.
    pub fn new(ctx: SystemContext) -> Self {
        Self {
            ctx,
            tool_router: Self::assembled_tool_router(),
        }
    }

    /// Return the full tool catalog without instantiating an `McpServer`.
    /// Composes the per-domain routers via `assembled_tool_router()` to list
    /// all tools.
    pub fn static_tool_catalog() -> Vec<rmcp::model::Tool> {
        Self::assembled_tool_router().list_all()
    }

    /// The tool names of each per-domain router, in `assembled_tool_router`
    /// order. Single source of truth for the runtime-derived name→domain map
    /// (`crate::mcp::tool_domains`) that the adaptive per-client tool surface
    /// gates on. Mirrors `assembled_tool_router()`; keep the two in lockstep —
    /// `tool_domains::tests::every_assembled_tool_has_exactly_one_domain` fails
    /// if a router is summed into the assembly but omitted here.
    pub(crate) fn domain_tool_names() -> Vec<(&'static str, Vec<String>)> {
        fn names(router: ToolRouter<McpServer>) -> Vec<String> {
            router
                .list_all()
                .into_iter()
                .map(|t| t.name.to_string())
                .collect()
        }
        vec![
            ("core", names(Self::router_core())),
            ("memory_crud", names(Self::router_memory_crud())),
            ("memory_search", names(Self::router_memory_search())),
            ("inventory", names(Self::router_inventory())),
            ("similarity", names(Self::router_similarity())),
            ("recommend", names(Self::router_recommend())),
            ("patterns", names(Self::router_patterns())),
            ("topics", names(Self::router_topics())),
            ("graph_core", names(Self::router_graph_core())),
            ("architecture", names(Self::router_architecture())),
            ("prediction", names(Self::router_prediction())),
            ("a2a", names(Self::router_a2a())),
            ("csm", names(Self::router_csm())),
            ("experiments", names(Self::router_experiments())),
            ("work_items_a", names(Self::router_work_items_a())),
            ("work_items_b", names(Self::router_work_items_b())),
            ("data_tables", names(Self::router_data_tables())),
            ("trajectory", names(Self::router_trajectory())),
            ("graph_advanced", names(Self::router_graph_advanced())),
            ("infotheory", names(Self::router_infotheory())),
            ("quality_evo", names(Self::router_quality_evo())),
            ("graph_func", names(Self::router_graph_func())),
            ("concurrency", names(Self::router_concurrency())),
            ("security", names(Self::router_security())),
            ("api_contract", names(Self::router_api_contract())),
            ("ml_embedding", names(Self::router_ml_embedding())),
            ("data_eng", names(Self::router_data_eng())),
            ("callgraph_evo", names(Self::router_callgraph_evo())),
            ("core_advanced", names(Self::router_core_advanced())),
            ("sema", names(Self::router_sema())),
            ("fuzzy", names(Self::router_fuzzy())),
            ("ontology", names(Self::router_ontology())),
            ("toolbox", names(Self::router_toolbox())),
            ("meta", names(Self::router_meta())),
        ]
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
        let project_hint = telemetry_project_from_json(&args);
        instrumented_tool_run(
            self.stats(),
            name,
            None,
            caller,
            &params_summary,
            None,
            None,
            project_hint,
            self.dispatch_named(name, args),
        )
        .await
    }

    /// Dispatch an inner tool on behalf of the `call_tool` meta-tool, attributing
    /// the durable telemetry row to the INNER tool name + the real caller (so the
    /// adaptive tool-policy learner sees true tool usage, not the `call_tool`
    /// wrapper). A 30 s budget matches the default MCP tool timeout.
    pub(crate) async fn dispatch_for_call_tool(
        &self,
        ctx: &RequestContext<RoleServer>,
        name: &str,
        args: serde_json::Value,
    ) -> Result<CallToolResult, McpError> {
        let caller = extract_caller(ctx);
        let request_id = Some(format!("{:?}", ctx.id));
        let mcp_session_id = extract_mcp_session_id(ctx);
        let project_hint = telemetry_project_from_json(&args);
        let params_summary = summarize_json(&args);
        instrumented_tool_run(
            self.stats(),
            name,
            Some(30),
            caller,
            &params_summary,
            request_id,
            mcp_session_id,
            project_hint,
            self.dispatch_named(name, args),
        )
        .await
    }

    /// Raw name→body dispatch shared by `call_tool_cli` and
    /// `dispatch_for_call_tool`. No instrumentation — each caller wraps it in
    /// `instrumented_tool_run` with the tool name + caller it wants recorded.
    async fn dispatch_named(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<CallToolResult, McpError> {
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
            // Developer-tool ("toolbox") catalog — all share the `tool_toolbox` module.
            "toolbox_search"             => toolbox_search(ToolboxSearchParams) in tool_toolbox,
            "toolbox_recommend"          => toolbox_recommend(ToolboxRecommendParams) in tool_toolbox,
            "toolbox_get"                => toolbox_get(ToolboxGetParams) in tool_toolbox,
            "toolbox_list"               => toolbox_list(ToolboxListParams) in tool_toolbox,
            "toolbox_refresh"            => toolbox_refresh(ToolboxRefreshParams) in tool_toolbox,
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
            // v31 — A2A/coordination conversation search (wraps memory_unified_search).
            "conversation_search"           => conversation_search(ConversationSearchParams),
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
            "work_summary"           => work_summary(WorkSummaryParams),
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
            // Topic analysis (portfolio)
            "project_topic_profile"    => project_topic_profile(ProjectTopicProfileParams),
            "topic_project_map"        => topic_project_map(TopicProjectMapParams),
            "project_topic_similarity" => project_topic_similarity(ProjectTopicSimilarityParams),
            "topic_cooccurrence"       => topic_cooccurrence(TopicCooccurrenceParams),
            "topic_coverage_gaps"      => topic_coverage_gaps(TopicCoverageGapsParams),
            "topic_owners"             => topic_owners(TopicOwnersParams),
            "topic_trends"             => topic_trends(TopicTrendsParams),
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
            "security_scan"          => security_scan(SecurityScanParams),
            // A2A inter-agent IPC bridge
            "a2a_send_task"          => a2a_send_task(A2aSendTaskParams),
            "a2a_get_task"           => a2a_get_task(A2aGetTaskParams),
            "a2a_subscribe_task"     => a2a_subscribe_task(A2aSubscribeTaskParams),
            "a2a_cancel_task"        => a2a_cancel_task(A2aCancelTaskParams),
            "a2a_register_agent"     => a2a_register_agent(A2aRegisterAgentParams),
            "a2a_list_agents"        => a2a_list_agents(A2aListAgentsParams),
            "a2a_active_agents"      => a2a_active_agents(A2aActiveAgentsParams),
            "a2a_send_message"       => a2a_send_message(A2aSendMessageParams),
            "a2a_inbox"              => a2a_inbox(A2aInboxParams),
            "a2a_reply_message"      => a2a_reply_message(A2aReplyMessageParams),
            "a2a_ack_message"        => a2a_ack_message(A2aAckMessageParams),
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
            // JSON data tables (share the data_tables module).
            "data_table_create"      => data_table_create(DataTableCreateParams) in data_tables,
            "data_table_alter"       => data_table_alter(DataTableAlterParams) in data_tables,
            "data_table_drop"        => data_table_drop(DataTableDropParams) in data_tables,
            "data_table_list"        => data_table_list(DataTableListParams) in data_tables,
            "data_table_describe"    => data_table_describe(DataTableDescribeParams) in data_tables,
            "data_table_insert"      => data_table_insert(DataTableInsertParams) in data_tables,
            "data_table_select"      => data_table_select(DataTableSelectParams) in data_tables,
            "data_table_update"      => data_table_update(DataTableUpdateParams) in data_tables,
            "data_table_delete"      => data_table_delete(DataTableDeleteParams) in data_tables,
            "data_table_aggregate"   => data_table_aggregate(DataTableAggregateParams) in data_tables,
            "data_table_report"      => data_table_report(DataTableReportParams) in data_tables,
            "data_table_search"      => data_table_search(DataTableSearchParams) in data_tables,
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
            "work_item_triage"       => work_item_triage(WorkItemTriageParams) in work_items,
            "work_item_resolve"      => work_item_resolve(WorkItemResolveParams) in work_items,
            // Work-item tracker Phase 2 — smart-views, next-action, assign, history, bulk.
            "work_item_view"            => work_item_view(WorkItemViewParams) in work_items,
            "work_item_next_actionable" => work_item_next_actionable(WorkItemNextActionableParams) in work_items,
            "work_item_assign"          => work_item_assign(WorkItemAssignParams) in work_items,
            "work_item_history"         => work_item_history(WorkItemHistoryParams) in work_items,
            "work_item_bulk"            => work_item_bulk(WorkItemBulkParams) in work_items,
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
            "work_item_link_commit"  => work_item_link_commit(WorkItemLinkCommitParams) in work_items,
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
            // Shadow-ASR interprocedural concurrency (ADR-011): registered in
            // `router_concurrency` for the MCP transport; these entries give the
            // CLI / `call_tool_cli` path the same reach (and the coverage gate
            // its hook). `tool_concurrency_deadlock.rs` exercises them.
            "deadlock_cycles"        => deadlock_cycles(DeadlockCyclesParams),
            "channel_deadlock"       => channel_deadlock(ChannelDeadlockParams),
            "lock_order_graph"       => lock_order_graph(LockOrderGraphParams),
            "sync_skeleton"          => sync_skeleton(SyncSkeletonParams),
            "concurrency_bottlenecks" => concurrency_bottlenecks(ConcurrencyBottlenecksParams),
            "concurrency_forecast"   => concurrency_forecast(ConcurrencyForecastParams),
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
            "quality_report"         => quality_report(QualityReportParams),
            // Phase 1 — trends & forecasting (quality-history trajectory)
            "quality_trend"          => quality_trend(QualityTrendParams),
            "quality_forecast"       => quality_forecast(QualityForecastParams),
            // Telemetry
            "mcp_tool_telemetry"     => mcp_tool_telemetry(McpToolTelemetryParams),
            "adoption_report"        => adoption_report(AdoptionReportParams),
            // Orientation / multi-axis tools previously omitted from the
            // dispatch — added so `call_tool_cli` can drive every #[tool]
            // method from harness tests. See `query_smoke_mcp_tools.rs`.
            "orient"                         => orient(OrientParams),
            "topic_hierarchy_fcm"            => topic_hierarchy_fcm(TopicHierarchyFcmParams),
            "dependency_health"              => dependency_health(DependencyHealthParams),
            "shotgun_surgery_fix"            => shotgun_surgery_fix(ShotgunSurgeryFixParams),
            "pr_scope_recommender"           => pr_scope(PrScopeRecommenderParams) in tool_pr_scope,
            "naming_consistency"             => naming_consistency(NamingConsistencyParams),
            "import_hygiene"                 => import_hygiene(ImportHygieneParams),
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
            "reindex"                        => reindex(ReindexParams),
            "active_clients"                 => active_clients(ActiveClientsParams),
            "cron_history"                   => cron_history(CronHistoryParams),
            "client_project_matrix"          => client_project_matrix(ClientProjectMatrixParams),
            "project_dependents"             => project_dependents(ProjectDependentsParams),
            "project_dependencies"           => project_dependencies(ProjectDependenciesParams),
            "coordinate_dependency_block"    => coordinate_dependency_block(CoordinateDependencyBlockParams),
            "coordination_respond"           => coordination_respond(CoordinationRespondParams),
            "suggest_worktree"               => suggest_worktree(SuggestWorktreeParams),
            // Ontology tools — CLI-dispatched so their `oracle_*`
            // regression tests (which drive `call_tool_cli`) can reach them.
            "ontology_create_concept"        => ontology_create_concept(OntologyCreateConceptParams) in tool_ontology,
            "ontology_invariants_for_file"   => ontology_invariants_for_file(OntologyInvariantsForFileParams) in tool_ontology,
            // Adaptive tool surface: catalog browse/search (the stateful
            // meta-tools enable_tools/disable_tools/call_tool need a live MCP
            // session/peer and are NOT CLI-dispatchable).
            "tool_catalog"                   => tool_catalog(ToolCatalogParams) in tool_meta,
        }, no_params: {
            "list_projects" => list_projects,
            "index_stats"   => index_stats,
            "pattern_catalog_stats" => pattern_catalog_stats in tool_software_patterns,
            "toolbox_stats" => toolbox_stats in tool_toolbox,
        })
    }
}

/// Full social-tool banner appended to the instructions catalog for claude-*
/// clients (claude-code, claude-cli). Surfaces the under-adopted tool families
/// with trigger-led "when to use" framing. See the adoption plan
/// `how-can-the-agents-replicated-lighthouse.md`.
const FULL_SOCIAL_BANNER: &str = "### Collaboration, memory, coordination & tracking (often under-used)\n\n\
COLLABORATION (A2A): when a task benefits from a second opinion, parallel specialists, or an \
adversarial check, delegate to peer agents — `a2a_pattern_sequential` (staged Planner→Critic→Solver), \
`a2a_pattern_mixture` (breadth: N specialists→summary), `a2a_pattern_deliberation` (hard problems, \
iterate to converge), `a2a_pattern_distillation` (compact teachable rationale). Discover peers with \
`a2a_list_agents` / `a2a_find_agents_by_specialty` (peers must be registered — run `pgmcp a2a-adapter` \
or enable `[a2a] autostart_adapters`); dispatch one-off work with `a2a_send_task` (+`a2a_subscribe_task`); \
record what worked with `a2a_report_outcome`.\n\n\
COORDINATION CONFORMANCE (CSM): after running an `a2a_pattern_*` tool, call `csm_validate_run(task_id)` \
to check the run against its coordination protocol and feed the learner; inspect contracts with \
`csm_list_protocols` / `csm_protocol_of_pattern` / `csm_show_projection`.\n\n\
MEMORY: before re-deriving project facts, query `memory_unified_search` / `memory_semantic_search` / \
`memory_hybrid_search`; persist durable facts (\"remember\", \"from now on\") with `memory_create_entities` \
+ `memory_add_observations`; trace relationships with `memory_neighbors` / `memory_path_search`; recall \
prior prompts and decisions with `recall_prompts` / `search_mandates`.\n\n\
LARGE-CONTEXT (RLM): when a question spans a whole file, module, or repo beyond one pass, use \
`a2a_pattern_recursive` (decompose → recurse → stitch; `rlm_depth` / `rlm_budget` tunable).\n\n\
WORK-ITEM TRACKER: track multi-step work — `work_item_create` (epic/story/task, parentable), \
`work_item_ingest_plan` (turn a plan into a tracked tree), `work_item_list` / `work_item_tree`, \
`work_item_set_status` (gated transitions), `work_item_record_progress`; for cross-agent work, \
`work_item_claim` / `claim_next` / `handoff`. Prefer this over ad-hoc TODOs for anything spanning \
more than one session.";

/// Terse social-tool banner for codex* clients (token-efficient — codex runs
/// with compact_json / default_brief).
const TERSE_SOCIAL_BANNER: &str = "### Under-used tool families\n\n\
Collaboration: `a2a_pattern_{sequential,mixture,deliberation,distillation}` + `a2a_send_task` / \
`a2a_find_agents_by_specialty` (peers need `pgmcp a2a-adapter` running). After a pattern run: \
`csm_validate_run(task_id)`. Memory: `memory_unified_search` to recall; `memory_create_entities` + \
`memory_add_observations` to persist durable facts. Large context: `a2a_pattern_recursive`. \
Multi-step work: `work_item_create` / `work_item_ingest_plan` / `work_item_claim` / `handoff`.";

/// The per-client social banner appended to the base instructions by the
/// `initialize` override. claude-* → full; codex* → terse; everything else
/// (generic / unknown) → none (catalog only). Both target clients reach this
/// via the MCP `initialize` handshake (the only per-client instruction surface
/// that reaches Codex, which has no prompt hook).
pub(crate) fn social_banner_for(client_name: &str) -> &'static str {
    let n = client_name.to_lowercase();
    if n.contains("claude") {
        FULL_SOCIAL_BANNER
    } else if n.contains("codex") {
        TERSE_SOCIAL_BANNER
    } else {
        ""
    }
}

/// Compose per-client instructions: the base catalog (from `get_info`) followed
/// by the social banner, if any. Kept pure so the composition (catalog
/// preservation + per-client variance) is unit-testable without an rmcp peer.
pub(crate) fn compose_instructions(base: &str, client_name: &str) -> String {
    let social = social_banner_for(client_name);
    if social.is_empty() {
        base.to_string()
    } else {
        format!("{base}\n\n{social}")
    }
}

// NOTE: `#[tool_handler]` is intentionally NOT used here. We hand-write its three
// generated methods so `list_tools` can apply per-client `description_overrides`:
// `call_tool` and `get_tool` are copied verbatim from the macro expansion
// (rmcp-macros-1.1.0/src/tool_handler.rs); `list_tools` is customized. Keep the
// two verbatim methods in sync if rmcp 1.1.0 is upgraded.
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .enable_resources()
                .enable_completions()
                .enable_logging()
                .enable_tasks()
                .build(),
        )
        .with_server_info(Implementation::new("pgmcp", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "pgmcp indexes the user's development workspaces into PostgreSQL+pgvector and \
             exposes ~330 tools for cross-project search, semantic queries, graph analysis, \
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
             TOOL DISCOVERY: you may be served a focused default working set rather than the \
             full catalog (the server learns each client's set from its own usage and grows it \
             on demand). If you need a capability you do not see in tools/list, call \
             `tool_catalog({query:\"...\"})` to find the right tool, then `enable_tools({names:[...]})` \
             to add it natively (it appears after your client refreshes tools/list), or \
             `call_tool({name, args})` to invoke it directly without enabling. Clients served the \
             full catalog can ignore this — every tool is already visible.\n\n\
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

    /// Per-client `initialize`. Captures peer info first — mirroring the rmcp
    /// `ServerHandler::initialize` default (`handler/server.rs`), which is
    /// REQUIRED: without `set_peer_info`, `peer_info()` stays `None` and
    /// `extract_caller` zeroes `client_name` in ALL telemetry. Then returns
    /// instructions composed of the base catalog (from `get_info`) plus a
    /// client-tailored social-tool banner (claude → full, codex → terse, else
    /// catalog-only). This is the only per-client instruction surface that
    /// reaches Codex (which has no prompt hook).
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        // Read the name before `set_peer_info` consumes the request by value.
        let client_name = request.client_info.name.clone();
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        let mut info = self.get_info();
        let base = info.instructions.take().unwrap_or_default();
        info.instructions = Some(compose_instructions(&base, &client_name));
        Ok(info)
    }

    // ── Tool dispatch (hand-written replacement for `#[tool_handler]`) ─────

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve the caller's rendering posture (output format / brief) BEFORE
        // moving `context` into the dispatch ctx, and install it as a
        // request-scoped task-local so tool bodies serialize in the caller's
        // preferred `OutputFormat` without threading a parameter through 300+
        // tool signatures. See `crate::mcp::client_profile::with_render_ctx`.
        let client = extract_caller(&context).client_name;
        let rc = crate::mcp::client_profile::RenderCtx::from_profile(
            self.ctx().client_profiles().for_client(&client),
        );
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        crate::mcp::client_profile::with_render_ctx(rc, self.tool_router.call(tcc))
            .await
            .map(|result| reencode_result_for_format(result, rc))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut tools = self.tool_router.list_all();
        // `peer_info()` is set by `initialize` before `tools/list`, so
        // `extract_caller` yields the real client name here; unknown clients fall
        // through to the `generic` profile.
        let client = extract_caller(&context).client_name;
        let profile = self.ctx().client_profiles().for_client(&client);
        // Adaptive per-client tool surface: `All` (claude-code) is a no-op — the
        // full catalog, byte-identical to the unfiltered router. `Learned` / `Fixed`
        // expose `mandatory_core ∪ learned_defaults(client) ∪ this session's
        // enable_tools overlay`. The session overlay is keyed by the mcp-session-id
        // header (None for stdio/CLI → no overlay).
        let session_enabled = extract_mcp_session_id(&context)
            .and_then(|sid| {
                self.ctx()
                    .tool_sessions()
                    .get(&sid)
                    .map(|s| s.enabled.clone())
            })
            .unwrap_or_default();
        self.ctx()
            .tool_policy()
            .retain_exposed(&mut tools, profile, &client, &session_enabled);
        // Per-client tool-description overrides (e.g. terser descriptions for
        // codex), applied to the survivors.
        profile.apply_description_overrides(&mut tools);
        Ok(ListToolsResult {
            tools,
            meta: None,
            next_cursor: None,
        })
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tool_router.get(name).cloned()
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

#[cfg(test)]
mod social_banner_tests {
    use super::{compose_instructions, social_banner_for};

    #[test]
    fn banner_is_per_client() {
        assert!(social_banner_for("claude-code").contains("COLLABORATION (A2A)"));
        assert!(social_banner_for("claude-cli").contains("COLLABORATION (A2A)"));
        assert!(social_banner_for("Claude Code").contains("COLLABORATION (A2A)"));
        assert!(social_banner_for("codex-mcp-client").contains("a2a_pattern_recursive"));
        assert!(social_banner_for("codex").contains("a2a_pattern_recursive"));
        assert!(social_banner_for("cursor").is_empty());
        assert!(social_banner_for("generic").is_empty());
        // The terse codex banner is shorter than the full claude banner.
        assert!(social_banner_for("codex").len() < social_banner_for("claude-code").len());
    }

    #[test]
    fn compose_preserves_catalog_and_varies_by_client() {
        let base = "BASE-CATALOG-MARKER";
        let claude = compose_instructions(base, "claude-code");
        let codex = compose_instructions(base, "codex-mcp-client");
        let generic = compose_instructions(base, "cursor");
        // (b) the base catalog survives for every client.
        assert!(claude.contains(base));
        assert!(codex.contains(base));
        assert!(generic.contains(base));
        // (a) per-client variance; generic/unknown gets catalog only.
        assert!(claude.contains("COLLABORATION (A2A)"));
        assert!(claude.len() > codex.len());
        assert_eq!(generic, base);
    }
}

#[cfg(test)]
mod telemetry_tests {
    use rmcp::model::{CallToolResult, Content};
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::{
        CallerInfo, instrumented_tool_run, normalize_telemetry_string, telemetry_project_from_json,
    };
    use crate::stats::tracker::StatsTracker;

    #[test]
    fn telemetry_project_from_json_normalizes_missing_or_empty_projects() {
        assert_eq!(
            telemetry_project_from_json(&json!({"project": " pgmcp "})),
            Some("pgmcp".to_string())
        );
        assert_eq!(telemetry_project_from_json(&json!({"project": ""})), None);
        assert_eq!(telemetry_project_from_json(&json!({"project": 42})), None);
        assert_eq!(normalize_telemetry_string(Some(" \t ")), None);
    }

    #[tokio::test]
    async fn instrumented_tool_run_enqueues_project_hint() {
        let stats = StatsTracker::new();
        let (tx, mut rx) = mpsc::channel(1);
        stats.set_telemetry_sender(tx);
        let caller = CallerInfo {
            client_name: "cli".to_string(),
            client_version: "test".to_string(),
            protocol_version: "n/a".to_string(),
        };

        let result = instrumented_tool_run(
            &stats,
            "semantic_search",
            None,
            caller,
            r#"{"project":"pgmcp"}"#,
            None,
            None,
            Some("pgmcp".to_string()),
            async { Ok(CallToolResult::success(vec![Content::text("ok")])) },
        )
        .await;

        assert!(result.is_ok());
        let row = rx.try_recv().expect("telemetry row must be enqueued");
        assert_eq!(row.tool, "semantic_search");
        assert_eq!(row.project.as_deref(), Some("pgmcp"));
        assert!(row.params_sha256.is_some());
        // Result-size telemetry: "ok" is 2 bytes → ~1 token.
        assert_eq!(row.result_bytes, Some(2));
        assert_eq!(row.result_tokens_est, Some(1));
    }

    #[test]
    fn reencode_compacts_pretty_json_only_for_compact_clients() {
        use crate::mcp::client_profile::{OutputFormat, RenderCtx};

        let pretty = serde_json::to_string_pretty(&json!({"a": 1, "b": [2, 3]})).unwrap();
        assert!(pretty.contains('\n'));
        let result = CallToolResult::success(vec![Content::text(pretty.clone())]);

        // CompactJson client: the pretty JSON is re-encoded compact.
        let compact_rc = RenderCtx {
            output_format: OutputFormat::CompactJson,
            default_brief: true,
            include_provenance: false,
        };
        let out = super::reencode_result_for_format(result.clone(), compact_rc);
        let text = out.content[0].as_text().unwrap().text.clone();
        assert!(
            !text.contains('\n'),
            "compact client must get newline-free JSON"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            json!({"a": 1, "b": [2, 3]}),
            "compaction must be lossless"
        );

        // Markdown client (claude-code): unchanged, byte-identical.
        let out_md = super::reencode_result_for_format(result.clone(), RenderCtx::default());
        assert_eq!(out_md.content[0].as_text().unwrap().text, pretty);

        // Non-JSON text under a compact client: left untouched.
        let prose = CallToolResult::success(vec![Content::text("not json\nat all")]);
        let out_prose = super::reencode_result_for_format(prose, compact_rc);
        assert_eq!(
            out_prose.content[0].as_text().unwrap().text,
            "not json\nat all"
        );
    }
}

// Cross-crate tool unit tests live under `pgmcp-testing/tests/` to avoid
// Cargo's cyclic-dev-dep limitation (pgmcp ↔ pgmcp-testing). See the
// note in `Cargo.toml`'s `[dev-dependencies]` block.
