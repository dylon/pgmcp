//! SOTA tail, A2A, CSM/MPST & experiment parameter types.
//!
//! Extracted verbatim from `server.rs` (B.2 god-file split). All structs
//! re-exported by `params/mod.rs` and, transitively, by `server.rs` so
//! `crate::mcp::server::<Name>Params` resolves for every tool body file.
#![allow(unused_imports)]

use super::*;
use rmcp::schemars;
use serde::Deserialize;

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
