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
        description = "Bridge a profiler's hot symbols to the static code graph: parse an agent-provided \
profile artifact (perf report stdio table, folded/collapsed flamegraph stacks, or a massif dump) and resolve \
each hot symbol to file:line joined with its function-level PageRank and complexity (cyclomatic / fan-in/out / \
panic-paths). Ranks targets by runtime intensity × call-graph centrality × complexity. Read-only — pgmcp parses \
the text and runs SELECTs; it never runs perf/valgrind. kind = perf | flamegraph | massif."
    )]
    async fn profile_ingest(
        &self,
        Parameters(params): Parameters<ProfileIngestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "profile_ingest",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_profile_ingest::tool_profile_ingest(self.ctx(), params),
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

    #[tool(
        description = "Crucible P9: the FROZEN, pre-registered Context-Tape 3×3×5 experiment \
(3 arms × 3 task families × 5 metrics) with a composite acceptance criterion frozen BEFORE any run \
(accuracy: treatment Welch-t > control; cost: TOST-equivalent within ±20%; p95 latency ≤ SLO; \
max-context-handled ≥ 2× baseline). Always echoes the frozen definition. open=true opens the experiment \
(locking the criterion). Supply real `cells` [{arm,family,metric,samples}] to record + decide against the \
frozen rule. The 3×3×5 EXECUTION is dataset-gated (OOLONG-Pairs/BrowseComp-Plus/LongBench-CodeQA + live local \
models); no measurement is fabricated. Promotion of a verified positive decision into memory is gated on \
[experiments] allow_promotion (default OFF) AND promote_to_obs."
    )]
    async fn experiment_preregister_context_tape(
        &self,
        Parameters(params): Parameters<ExperimentPreregisterContextTapeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "experiment_preregister_context_tape",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_experiments::tool_experiment_preregister_context_tape(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    // ── Thread 5b — experiment-API hardening ───────────────────────────────
    // EXPERIMENT subsystem only: these never touch the work-item tracker or post
    // →verified evidence (the self-verification loophole was reverted 2026-06-20).

    #[tool(
        description = "Store the paired-corpus 2×2 (both_correct, control_only, treatment_only, both_wrong) for a \
(experiment, hypothesis, metric) — the correct representation for classification/recall benchmarks where the two \
arms score the SAME cases — and return the SERVER-COMPUTED McNemar verdict (statistic, p-value, discordant count, \
effect, exact-vs-χ², significant-at-0.05). The agent supplies counts; the daemon computes the test (never asserts \
it). hypothesis_id is required (the 2×2 dedupes per-hypothesis)."
    )]
    async fn experiment_record_paired_binary_counts(
        &self,
        Parameters(params): Parameters<ExperimentRecordPairedBinaryCountsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "experiment_record_paired_binary_counts",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_experiments::tool_experiment_record_paired_binary_counts(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Seal a measurement run for use in a decision: compute + store its tamper-evident SHA-256 \
samples digest, set status='finalized', and append to the immutable run-status audit trail. Idempotent. Returns \
{samples_digest, sample_count, status}. Use before experiment_decide when you want the run's data sealed."
    )]
    async fn experiment_finalize_run(
        &self,
        Parameters(params): Parameters<ExperimentFinalizeRunParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "experiment_finalize_run",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_experiments::tool_experiment_finalize_run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Audited EXCLUSION of a run from decisions (status 'invalid' or 'superseded' ONLY; a \
non-empty reason is REQUIRED). The anti-cherry-pick guardrail: any rendered decision that consumed the run is \
RE-OPENED (its hypothesis verdict reverts to pending), so excluding unfavorable data after a decision can never \
silently keep the favourable verdict. Returns {old_status, new_status, reopened_decisions}."
    )]
    async fn experiment_set_run_status(
        &self,
        Parameters(params): Parameters<ExperimentSetRunStatusParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "experiment_set_run_status",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_experiments::tool_experiment_set_run_status(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Ingest benchmark samples from an artifact FILE parsed SERVER-SIDE (CSV or JSONL), so you \
pass a path instead of a huge inline payload. The path is resolved relative to the working directory, \
canonicalized, and rejected if it escapes that root (path-traversal safety — no reading /etc/*). Extracts the \
numeric value_column → samples (unit_key from unit_key_columns; optional arm_column splits rows into per-arm \
runs; is_warmup_column flags warm-ups; filters select rows). Non-numeric/empty values are skipped + reported. \
Returns {runs:[{arm,run_id,inserted_samples}], skipped}."
    )]
    async fn experiment_record_measurement_from_artifact(
        &self,
        Parameters(params): Parameters<ExperimentRecordMeasurementFromArtifactParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "experiment_record_measurement_from_artifact",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_experiments::tool_experiment_record_measurement_from_artifact(
                self.ctx(),
                params,
            ),
        )
        .await
    }
}
