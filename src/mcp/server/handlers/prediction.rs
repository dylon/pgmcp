//! Bug-prediction, tech-debt & cron-trigger handlers.
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

#[rmcp::tool_router(router = router_prediction, vis = "pub(crate)")]
impl McpServer {
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
            crate::mcp::tools::tool_bug_prediction::tool_bug_prediction(self.ctx(), params),
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
            crate::mcp::tools::tool_technical_debt_analysis::tool_technical_debt_analysis(
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
            crate::mcp::tools::tool_anomaly_detection::tool_anomaly_detection(self.ctx(), params),
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
            crate::mcp::tools::tool_code_on_fire::tool_code_on_fire(self.ctx(), params),
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
            crate::mcp::tools::tool_documented_tech_debt::tool_documented_tech_debt(
                self.ctx(),
                params,
            ),
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
            crate::mcp::tools::tool_trigger_cron::tool_trigger_cron(self.ctx(), params),
        )
        .await
    }
}
