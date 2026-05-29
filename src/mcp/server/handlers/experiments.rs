//! Scientific-experiment subsystem & outcome-report handlers.
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

#[rmcp::tool_router(router = router_experiments, vis = "pub(crate)")]
impl McpServer {
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
            crate::mcp::tools::tool_a2a_report_outcome::tool_a2a_report_outcome(self.ctx(), params),
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
            crate::mcp::tools::tool_experiments::tool_experiment_open(self.ctx(), params),
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
            crate::mcp::tools::tool_experiments::tool_experiment_protocol(self.ctx(), params),
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
            crate::mcp::tools::tool_experiments::tool_experiment_record_measurement(
                self.ctx(),
                params,
            ),
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
            crate::mcp::tools::tool_experiments::tool_experiment_decide(self.ctx(), params),
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
            crate::mcp::tools::tool_experiments::tool_experiment_search(self.ctx(), params),
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
            crate::mcp::tools::tool_experiments::tool_experiment_get(self.ctx(), params),
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
            crate::mcp::tools::tool_experiments::tool_experiment_list(self.ctx(), params),
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
            crate::mcp::tools::tool_experiments::tool_experiment_timeline(self.ctx(), params),
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
            crate::mcp::tools::tool_experiments::tool_experiment_log_artifact(self.ctx(), params),
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
            crate::mcp::tools::tool_experiments::tool_experiment_render_ledger(self.ctx(), params),
        )
        .await
    }
}
