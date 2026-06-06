//! Architecture & design-quality handlers.
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

#[rmcp::tool_router(router = router_architecture, vis = "pub(crate)")]
impl McpServer {
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
            crate::mcp::tools::tool_coupling_cohesion_report::tool_coupling_cohesion_report(
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
            crate::mcp::tools::tool_architecture_violations::tool_architecture_violations(
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
            crate::mcp::tools::tool_design_smell_detection::tool_design_smell_detection(
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
            crate::mcp::tools::tool_architecture_quality::tool_architecture_quality(
                self.ctx(),
                params,
            ),
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
        let project_hint = Some(params.project.clone());
        instrumented_tool_wrap_with_project(
            self.stats(),
            "design_metrics",
            30,
            &_ctx,
            &summarize_debug(&params),
            project_hint,
            crate::mcp::tools::tool_design_metrics::tool_design_metrics(self.ctx(), params),
        )
        .await
    }
}
