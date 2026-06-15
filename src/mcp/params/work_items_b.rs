//! Work-item tracker tail, trajectory & cron parameter types (part B).
//!
//! Extracted verbatim from `server.rs` (B.2 god-file split). All structs
//! re-exported by `params/mod.rs` and, transitively, by `server.rs` so
//! `crate::mcp::server::<Name>Params` resolves for every tool body file.
#![allow(unused_imports)]

use super::*;
use rmcp::schemars;
use serde::Deserialize;

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
pub struct WorkItemTriageParams {
    #[schemars(description = "The bug's public_id to confirm (triage → confirmed)")]
    pub public_id: String,
    #[schemars(
        description = "The tracker user_token (user-authority gate; an agent cannot confirm a bug)"
    )]
    pub user_token: String,
    #[schemars(
        description = "Severity (impact): critical|high|medium|low. Required to confirm unless already set."
    )]
    pub severity: Option<String>,
    #[schemars(
        description = "Reproduction steps. Required to confirm unless already recorded on the bug."
    )]
    pub reproduction_steps: Option<String>,
    #[schemars(description = "Root-cause analysis captured during triage (optional)")]
    pub root_cause: Option<String>,
    #[schemars(description = "Who performed the triage (default 'user')")]
    pub triaged_by: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemResolveParams {
    #[schemars(description = "The bug's public_id to close without a fix (→ cancelled)")]
    pub public_id: String,
    #[schemars(description = "The tracker user_token (user-authority gate)")]
    pub user_token: String,
    #[schemars(
        description = "Resolution: wont_fix | duplicate | cannot_reproduce | by_design ('fixed' comes from the verify path, not here)"
    )]
    pub resolution: String,
    #[schemars(
        description = "Why it is being closed this way (required; recorded in the append-only audit)"
    )]
    pub reason: String,
    #[schemars(
        description = "For resolution=duplicate: the public_id this duplicates (records a 'duplicates' relation)"
    )]
    pub duplicate_of: Option<String>,
    #[schemars(description = "Version/commit where it was fixed, if known (optional)")]
    pub fixed_in_version: Option<String>,
    #[schemars(description = "Root-cause analysis (optional)")]
    pub root_cause: Option<String>,
    #[schemars(description = "Who authorized the resolution (default 'user')")]
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

// ── Phase 2 — tracker ergonomics & next-action (views, assign, history, bulk) ──

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemViewParams {
    #[schemars(
        description = "Smart-view name (closed set): my-work | needs-triage | overdue | blocked | next-actionable. my-work filters on the durable assignee (the caller's agent id when assignee is omitted)."
    )]
    pub view: String,
    #[schemars(
        description = "For my-work: the assignee to scope to (auto-filled from the MCP client name when omitted)."
    )]
    pub assignee: Option<String>,
    #[schemars(description = "Max rows (default 50, clamped 1..=1000)")]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemNextActionableParams {
    #[schemars(
        description = "Restrict to the subtree under this plan public_id (omit = workspace-wide)."
    )]
    pub plan_public_id: Option<String>,
    #[schemars(description = "Restrict to items owned by this durable assignee (optional).")]
    pub assignee: Option<String>,
    #[schemars(description = "Max rows (default 50, clamped 1..=1000)")]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemAssignParams {
    #[schemars(description = "The item's public_id to (re)assign or unassign.")]
    pub public_id: String,
    #[schemars(
        description = "The durable owner agent id. Omit (or pass empty) to UNASSIGN. assignee is durable ownership intent (1:1, never auto-cleared) — distinct from the ephemeral claimed_by execution lease."
    )]
    pub assignee: Option<String>,
    #[schemars(
        description = "Who performed the assignment (auto-filled from the MCP client name)."
    )]
    pub assigned_by: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemHistoryParams {
    #[schemars(description = "The item's public_id whose unified timeline to fetch.")]
    pub public_id: String,
    #[schemars(description = "Max events (default 100, clamped 1..=1000)")]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemBulkParams {
    #[schemars(
        description = "Bulk operation (closed set): set_status | tag | untag | reprioritize | assign. set_status loops through the per-item transition chokepoint (legality + auto-unblock fire per item)."
    )]
    pub op: String,
    #[schemars(
        description = "Explicit target public_ids. Either this OR `view` must select targets (capped at 500)."
    )]
    pub public_ids: Option<Vec<String>>,
    #[schemars(
        description = "Alternatively, select targets by smart-view (my-work | needs-triage | overdue | blocked | next-actionable). Ignored when public_ids is given."
    )]
    pub view: Option<String>,
    #[schemars(description = "For op=set_status: the target status to transition each item to.")]
    pub status: Option<String>,
    #[schemars(description = "For op=tag/untag: the tag label/slug.")]
    pub tag: Option<String>,
    #[schemars(
        description = "For op=assign: the durable owner agent id (omit/empty to unassign each)."
    )]
    pub assignee: Option<String>,
    #[schemars(description = "For op=reprioritize: the new priority to set on each item.")]
    pub priority: Option<i32>,
    #[schemars(
        description = "For op=set_status: an optional reason recorded in the append-only status history."
    )]
    pub reason: Option<String>,
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
        description = "Cron job name: \"symbol-extraction\" | \"call-graph\" | \"function-metrics\" | \"fuzzy-sync\" | \"a2a-reflect\" | \"msm-calibrate\" | \"graph-analysis\" | \"target-cleanup\" | \"security-scan\" | \"findings-promotion\" | \"topic-clustering\" | \"code-raptor\" | \"topic-dendrogram\". Use symbol-extraction first to populate file_symbols (needed by dead_code_reachability and naming_consistency), then call-graph to populate symbol_references edges, then function-metrics for cyclomatic/cognitive/Halstead/NPath/MI. fuzzy-sync rebuilds the per-project symbol/path/commit/mandate fuzzy tries from PG. topic-clustering recomputes topics (graph-hybrid engine + per-project + global roll-up + degeneracy gate + LLM labels) for discover_topics; code-raptor rebuilds code_summary_tree; topic-dendrogram rebuilds topic_dendrograms; memory-raptor rebuilds memory_summary_tree (loads the local LLM). Optionally set `project` to scope symbol-extraction / call-graph / function-metrics to a single project."
    )]
    pub job: String,
    /// Optional project (name or numeric id) to scope a per-project job to.
    #[serde(default)]
    #[schemars(
        description = "Optional project name or numeric id. When set, the symbol-extraction / call-graph / function-metrics jobs run for just that project — keeping a manual trigger within its time budget on a large workspace — instead of looping every project. Ignored by jobs that are not per-project (fuzzy-sync / a2a-reflect / msm-calibrate / graph-analysis)."
    )]
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CronHistoryParams {
    /// Optional cron-job-name filter for the `recent` list.
    #[serde(default)]
    #[schemars(
        description = "Optional cron job name to filter the recent-runs list (e.g. \"topic-clustering\", \"symbol-extraction\"). Omit to see recent runs across all jobs. The per-job rollup always covers every job."
    )]
    pub job: Option<String>,
    /// Cap on the number of recent rows returned (1..=500, default 50).
    #[serde(default)]
    #[schemars(
        description = "Max recent run rows to return (clamped to 1..=500; default 50). The per-job rollup is unaffected."
    )]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemLinkCommitParams {
    #[schemars(description = "The item's public_id to link to a commit / PR / branch")]
    pub public_id: String,
    #[schemars(
        description = "The reference value: a commit SHA (full or unique prefix), a PR number, or a branch name"
    )]
    pub ref_value: String,
    #[schemars(
        description = "Link type: commit | pr | branch. Omit to infer from ref_value shape (hex≥7 ⇒ commit, digits ⇒ pr, else branch)."
    )]
    pub link_type: Option<String>,
    #[schemars(
        description = "Project name to scope a commit lookup against (defaults to the item's own project)"
    )]
    pub project: Option<String>,
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
