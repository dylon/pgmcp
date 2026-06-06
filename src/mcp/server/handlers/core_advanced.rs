//! Advanced summarization, scorecard & adoption handlers.
//!
//! Tool handlers extracted verbatim from `server.rs` (B.3 god-file split).
//! Only the relative `super::tools::` path was rewritten to the absolute
//! `crate::mcp::tools::`; bodies are otherwise identical. The per-block
//! router is composed in `server.rs` via `assembled_tool_router()`.
#![allow(clippy::too_many_lines)]

use crate::mcp::server::McpServer;
use crate::mcp::server::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};

#[rmcp::tool_router(router = router_core_advanced, vis = "pub(crate)")]
impl McpServer {
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
        instrumented_tool_wrap_with_project(
            self.stats(),
            "hybrid_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            params.project.clone(),
            crate::mcp::tools::tool_hybrid_search::tool_hybrid_search(self.ctx(), params),
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
            crate::mcp::tools::tool_code_summarize::tool_code_summarize(self.ctx(), params),
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
            crate::mcp::tools::tool_engineering_scorecard::tool_engineering_scorecard(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Graded, three-pillar (Engineering / Architecture / Security) quality report \
that enumerates every issue pgmcp's analysis tools find for a project, rendered as GitHub Markdown \
(default), Org-mode, LaTeX, HTML, plain text, or JSON. \
USE WHEN: you want a single human-readable audit of a project's overall soundness, a graded \
scorecard with enumerated findings, a shareable report (LaTeX/HTML), or machine-readable findings \
(format=json) for tooling. \
DO NOT USE WHEN: you only need one analysis — call that tool directly (e.g. secret_detection, \
complexity_hotspots). \
Aggregates ~44 finding collectors + both scorecards into pillar GPAs (A-F), a worst-files roll-up, \
a per-pillar GPA trend, and an appendix of which tools ran. Best after the graph-analysis, \
symbol-extraction, function-metrics, and topic-clustering crons have populated metrics (pass \
refresh_crons to force them)."
    )]
    async fn quality_report(
        &self,
        Parameters(params): Parameters<QualityReportParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // Outer budget of 600s (NOT the scorecards' 30s): this tool fans out
        // over ~44 collectors, each with its own inner per-tool timeout.
        instrumented_tool_wrap(
            self.stats(),
            "quality_report",
            600,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_quality_report::tool_quality_report(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // Phase 1: Trends & forecasting (quality-history trajectory)
    // ========================================================================

    #[tool(
        description = "Quality TREND: the per-pillar GPA trajectory (Engineering / Architecture / \
Security / overall) over a lookback window, read from quality_report_history (populated by the \
`quality-history` cron). \
USE WHEN: you want to see how a project's health is MOVING — is tech-debt rising, is the \
Architecture GPA recovering — rather than a single snapshot (`engineering_scorecard` / \
`quality_report`). \
DO NOT USE WHEN: you only need the current grade (call `engineering_scorecard`) or a forward \
projection of when it crosses a threshold (call `quality_forecast`). \
Returns the timestamped samples, an EWMA-smoothed overall line (so a single stale-cron run is \
not a spike), and the first→last delta per pillar. Needs at least two snapshots for a delta."
    )]
    async fn quality_trend(
        &self,
        Parameters(params): Parameters<QualityTrendParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "quality_trend",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_quality_trend::tool_quality_trend(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Quality FORECAST: fit an OLS slope over the overall-GPA history and project \
when, on that trajectory, the project crosses a GPA threshold (default 2.0 = the C-grade floor). \
USE WHEN: you want a forward-looking 'debt hits a C in N weeks' signal to prioritize cleanup, or \
to confirm a declining trend will (or won't) cross a line. \
DO NOT USE WHEN: you only need the current grade (`engineering_scorecard`) or the raw series \
(`quality_trend`). \
Returns current_overall, slope_per_day, slope_per_week, weeks_to_threshold (null when flat / \
improving / already past, with an explanatory note), and the threshold. Degrades gracefully on \
short/empty history — never errors on a thin series."
    )]
    async fn quality_forecast(
        &self,
        Parameters(params): Parameters<QualityForecastParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "quality_forecast",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_quality_forecast::tool_quality_forecast(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Adoption telemetry: how often each under-used tool family \
(A2A, CSM coordination-conformance, memory, RLM, work-items) is actually called, by client and \
(where available) by session, read straight from mcp_tool_calls. \
USE WHEN you want to baseline or measure lift in adoption of the social/coordination/memory/\
recursive/work-tracking tools. Restricted to real clients (claude-code, codex-mcp-client, \
claude-cli); pgmcp's own CLI self-calls are excluded. Per-session rates only populate for calls \
made after the mcp_session_id telemetry fix. format=json (default) | markdown."
    )]
    async fn adoption_report(
        &self,
        Parameters(params): Parameters<AdoptionReportParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "adoption_report",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_adoption_report::tool_adoption_report(self.ctx(), params),
        )
        .await
    }
}
