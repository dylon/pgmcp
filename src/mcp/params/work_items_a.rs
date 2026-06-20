//! Experiment tail, work-item tracker & plan-definition parameter types (part A).
//!
//! Extracted verbatim from `server.rs` (B.2 god-file split). All structs
//! re-exported by `params/mod.rs` and, transitively, by `server.rs` so
//! `crate::mcp::server::<Name>Params` resolves for every tool body file.
#![allow(unused_imports)]

use super::*;
use rmcp::schemars;
use serde::Deserialize;

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
pub struct ProfileIngestParams {
    #[schemars(description = "Project name to resolve hot symbols against")]
    pub project: String,
    #[schemars(
        description = "The raw profile text (perf report stdio table, folded/collapsed stacks, or massif dump)"
    )]
    pub content: String,
    #[schemars(description = "Profile format: perf | flamegraph | massif")]
    pub kind: String,
    #[schemars(description = "Max hot symbols to resolve (default 25, max 200)")]
    pub limit: Option<i32>,
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
        description = "Item kind: plan | goal | epic | task | sub_task | todo | fixme | bug | idea | note | question | nice_to_have | action_item | experiment. kind=bug is born in 'triage' and supports severity + structured bug fields."
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
    #[schemars(
        description = "Bug severity (impact): critical | high | medium | low. Meaningful for kind=bug; when set without an explicit priority it seeds a default priority."
    )]
    pub severity: Option<String>,
    #[schemars(description = "Bug: how to reproduce (kind=bug)")]
    pub reproduction_steps: Option<String>,
    #[schemars(description = "Bug: expected behavior (kind=bug)")]
    pub expected_behavior: Option<String>,
    #[schemars(description = "Bug: actual (observed) behavior (kind=bug)")]
    pub actual_behavior: Option<String>,
    #[schemars(description = "Bug: environment — OS / runtime / config (kind=bug)")]
    pub environment: Option<String>,
    #[schemars(description = "Bug: version/commit where the defect was found (kind=bug)")]
    pub affected_version: Option<String>,
    #[schemars(description = "Bug: whether this is a regression (kind=bug)")]
    pub is_regression: Option<bool>,
    #[schemars(description = "Bug: who reported it (kind=bug)")]
    pub reported_by: Option<String>,
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
    #[schemars(description = "Bug severity (impact): critical|high|medium|low (omit to keep)")]
    pub severity: Option<String>,
    #[schemars(description = "Bug: reproduction steps (omit to keep)")]
    pub reproduction_steps: Option<String>,
    #[schemars(description = "Bug: expected behavior (omit to keep)")]
    pub expected_behavior: Option<String>,
    #[schemars(description = "Bug: actual behavior (omit to keep)")]
    pub actual_behavior: Option<String>,
    #[schemars(description = "Bug: environment (omit to keep)")]
    pub environment: Option<String>,
    #[schemars(description = "Bug: affected version (omit to keep)")]
    pub affected_version: Option<String>,
    #[schemars(description = "Bug: fixed-in version (omit to keep)")]
    pub fixed_in_version: Option<String>,
    #[schemars(description = "Bug: root cause (omit to keep)")]
    pub root_cause: Option<String>,
    #[schemars(description = "Bug: regression flag (omit to keep)")]
    pub is_regression: Option<bool>,
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
