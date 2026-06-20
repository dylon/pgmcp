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
        description = "Combined INTER + INTRA-project architectural quality over the hierarchical \
rollup (ADR-027): the workspace summary, per-group summaries, and per-project Martin metrics \
(instability / abstractness / distance-from-main-sequence + an architecture_quality_score), worst \
first. USE WHEN you want the whole-workspace architecture picture across projects, not one project. \
Backed by project_metrics / hier_group_metrics (graph-analysis rollup); rebuild=true re-aggregates \
the group+workspace levels. Returns {workspace, groups[], projects[]}."
    )]
    async fn workspace_architecture_quality(
        &self,
        Parameters(params): Parameters<WorkspaceArchitectureQualityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "workspace_architecture_quality",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_workspace_architecture_quality::tool_workspace_architecture_quality(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Verify the strict (extensive-sum) composition laws of the Containment functor \
over the hierarchical rollup (ADR-028): the workspace total of each extensive metric (file counts) \
must equal the sum over projects. USE WHEN you want to confirm the inter/intra rollup is internally \
consistent — a violation is a data-integrity bug, not a metric choice. rebuild=true re-aggregates \
first. Returns {laws_checked, ok, violations[]}."
    )]
    async fn categorical_lint(
        &self,
        Parameters(params): Parameters<CategoricalLintParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "categorical_lint",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_categorical_lint::tool_categorical_lint(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Inter-project dependency coupling over the multi-ecosystem \
project_dependencies graph (ADR-027): per-project efferent (Ce) / afferent (Ca) coupling + \
instability, the most-depended-upon \"god projects\" (highest Ca first), and cross-project \
dependency CYCLES (SCCs). USE WHEN you want to identify dependency coupling and cycles ACROSS \
projects. Run the project-deps cron first to populate edges. Returns {project_count, edge_count, \
cross_project_cycles[], projects[]}."
    )]
    async fn cross_project_coupling(
        &self,
        Parameters(params): Parameters<CrossProjectCouplingParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "cross_project_coupling",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_cross_project_coupling::tool_cross_project_coupling(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Pullback over the project-dependency category (ADR-028): projects that BOTH \
project_a and project_b depend on (shared upstream). Returns {count, common_dependencies}."
    )]
    async fn common_dependency(
        &self,
        Parameters(params): Parameters<CommonDependencyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "common_dependency",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_category2::tool_common_dependency(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Pushout over the project-dependency category (ADR-028): projects that depend \
on BOTH project_a and project_b (shared integrators/consumers). Returns {count, integration_points}."
    )]
    async fn integration_point(
        &self,
        Parameters(params): Parameters<IntegrationPointParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "integration_point",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_category2::tool_integration_point(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Functorial impact (ADR-028): per group, the gap between the intensive (lax) \
unweighted-mean rollup and a size-weighted mean — where collapsing the level loses information \
(a few large projects dominate). Returns {count, impacts}."
    )]
    async fn functorial_impact(
        &self,
        Parameters(params): Parameters<FunctorialImpactParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "functorial_impact",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_category2::tool_functorial_impact(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Effect functor (ADR-028): the Call → effect-set monoid — the effect monoid \
generators (distinct effects) and the most effectful symbols. Returns {monoid_generators, \
most_effectful_symbols}."
    )]
    async fn effect_functor(
        &self,
        Parameters(params): Parameters<EffectFunctorParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "effect_functor",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_category3::tool_effect_functor(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Naturality gap (ADR-028): import edges whose endpoints are semantically \
distant (the import and semantic functors disagree) — architectural erosion / leaky abstractions. \
Returns {gap_count, gaps}."
    )]
    async fn naturality_gap(
        &self,
        Parameters(params): Parameters<NaturalityGapParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "naturality_gap",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_category3::tool_naturality_gap(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Colimit view (ADR-028): the unified memory/code graph as a colimit of its \
per-source diagrams — node-type objects + (from_type, edge_type, to_type) component arms. Returns \
{objects, diagram_components}."
    )]
    async fn colimit_view(
        &self,
        Parameters(params): Parameters<ColimitViewParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "colimit_view",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_category3::tool_colimit_view(self.ctx(), params),
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
