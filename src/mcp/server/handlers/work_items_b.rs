//! Work-item / plan tracker handlers (part B: verify, claim, links, export).
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

#[rmcp::tool_router(router = router_work_items_b, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Record evidence for an acceptance criterion. NOTE: MCP-recorded evidence is \
source='manual' and CANNOT satisfy the verified gate (agents cannot self-verify) — trusted evidence comes \
from CI / the Stop-hook (REST) or the experiment engine."
    )]
    async fn work_item_record_evidence(
        &self,
        Parameters(params): Parameters<WorkItemRecordEvidenceParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_record_evidence",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_record_evidence(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Attempt the gatekeeper →verified transition for an item; succeeds only when every \
required criterion has passing, trusted-source evidence, else returns the explanatory refusal. The item must \
be in claimed_done or verifying."
    )]
    async fn work_item_attempt_verify(
        &self,
        Parameters(params): Parameters<WorkItemAttemptVerifyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_attempt_verify",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_attempt_verify(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "USER-only: defer (explicitly skip) an item so it is excluded from completion \
roll-up. Requires the tracker user_token — an agent CANNOT self-defer (no token; →deferred has no agent arm \
in the transition matrix). Records an append-only scope-negotiation."
    )]
    async fn work_item_defer(
        &self,
        Parameters(params): Parameters<WorkItemDeferParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_defer",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_defer(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "USER-only: reinstate a deferred item (deferred → in_progress). Requires the tracker \
user_token."
    )]
    async fn work_item_reinstate(
        &self,
        Parameters(params): Parameters<WorkItemReinstateParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_reinstate",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_reinstate(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "USER-only: triage-confirm a reported bug (triage → confirmed) so agents may work it. \
Requires the tracker user_token — an agent CANNOT confirm a bug (no token; →confirmed has no agent arm in the \
transition matrix). A severity and reproduction_steps must be present (pass them here, or set them earlier via \
work_item_create/update). Records the triage milestone (triaged_at/by + optional root_cause). Returns \
{item, bug_details}."
    )]
    async fn work_item_triage(
        &self,
        Parameters(params): Parameters<WorkItemTriageParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_triage",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_triage(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "USER-only: resolve/close a bug WITHOUT a code fix (→ cancelled) with a categorized \
resolution (wont_fix|duplicate|cannot_reproduce|by_design). Requires the tracker user_token. For \
resolution=duplicate, pass duplicate_of to record a 'duplicates' relation. ('fixed' is reached via \
work_item_attempt_verify, not here.) Records an append-only scope-negotiation. Returns {item, bug_details, \
resolution}."
    )]
    async fn work_item_resolve(
        &self,
        Parameters(params): Parameters<WorkItemResolveParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_resolve",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_resolve(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Auto-translate an agent's markdown plan into a tracked work_items subtree \
(headings→plan/epic/task/sub_task, checklists→todos, numbered→sub_tasks, 'acceptance:' lines→criteria). \
Idempotent on re-ingest — preserves status/progress. Optionally validates against a plan definition."
    )]
    async fn work_item_ingest_plan(
        &self,
        Parameters(params): Parameters<WorkItemIngestPlanParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_ingest_plan",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_ingest_plan(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Promote a discovered code marker (TODO/FIXME/HACK/…) into a tracked work item \
(fixme/todo). Idempotent on the marker text+location. USE WHEN turning documented_tech_debt findings into \
trackable items."
    )]
    async fn work_item_promote_marker(
        &self,
        Parameters(params): Parameters<WorkItemPromoteMarkerParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_promote_marker",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_promote_marker(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Atomically claim a work item to work on it (open→in_progress). USE WHEN starting work \
on a shared plan so other agents see it's taken. Returns claimed:false (with the current owner) if another \
agent holds it, it's blocked by a dependency, or it's terminal. Leases auto-expire (crash-safe)."
    )]
    async fn work_item_claim(
        &self,
        Parameters(mut params): Parameters<WorkItemClaimParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.agent_id.is_none() {
            params.agent_id = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "work_item_claim",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_claim(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Claim the next available (unclaimed, unblocked, ready) item, top by priority/score — \
optionally within a plan subtree. The fan-out execution primitive: N agents each get a distinct item. \
Returns claimed:false when the queue is empty."
    )]
    async fn work_item_claim_next(
        &self,
        Parameters(mut params): Parameters<WorkItemClaimNextParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.agent_id.is_none() {
            params.agent_id = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "work_item_claim_next",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_claim_next(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Release your claim on an item (owner-gated). USE WHEN you stop working on a claimed \
item so another agent can pick it up."
    )]
    async fn work_item_release(
        &self,
        Parameters(mut params): Parameters<WorkItemReleaseParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.agent_id.is_none() {
            params.agent_id = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "work_item_release",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_release(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Hand off your claim on an item to another agent (owner-gated re-key). USE WHEN \
delegating a claimed item to a peer agent."
    )]
    async fn work_item_handoff(
        &self,
        Parameters(mut params): Parameters<WorkItemHandoffParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.agent_id.is_none() {
            params.agent_id = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "work_item_handoff",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_handoff(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Heartbeat: mark this agent active and renew the leases on all items it holds (one \
call). USE WHEN working a long-running claimed item so the lease doesn't expire and let another agent steal it."
    )]
    async fn agent_heartbeat(
        &self,
        Parameters(mut params): Parameters<AgentHeartbeatParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.agent_id.is_none() {
            params.agent_id = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "agent_heartbeat",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_agent_heartbeat(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Who currently owns a work item + its claim/handoff history. USE WHEN checking whether \
an item is being worked and by whom before claiming it."
    )]
    async fn work_item_who_owns(
        &self,
        Parameters(params): Parameters<WorkItemWhoOwnsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_who_owns",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_who_owns(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "What an agent is doing (its presence + currently-claimed items + workload), or — with \
no agent_id — the active-agent roster ('who is working'). USE WHEN coordinating multiple agents."
    )]
    async fn agent_activity(
        &self,
        Parameters(params): Parameters<AgentActivityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "agent_activity",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_agent_activity(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "The workspace (or plan-scoped) activity feed: recent progress + claim/handoff events, \
newest first, agent-attributed. USE WHEN reviewing 'what is happening' across the tracker or on a plan."
    )]
    async fn work_item_activity(
        &self,
        Parameters(params): Parameters<WorkItemActivityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_activity",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_activity(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Link two work items with a typed relation (blocks | depends_on | relates_to | \
duplicates | supersedes | derived_from). The ordering relations (depends_on/blocks) are REJECTED if they \
would create a dependency cycle (an unschedulable loop). USE WHEN recording that one item blocks/depends-on \
another, duplicates it, or supersedes it."
    )]
    async fn work_item_link(
        &self,
        Parameters(mut params): Parameters<WorkItemLinkParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.created_by.is_none() {
            params.created_by = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "work_item_link",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_link(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Remove a typed relation between two work items. Returns {removed: bool}. USE WHEN a \
dependency/blocks/duplicates link no longer holds."
    )]
    async fn work_item_unlink(
        &self,
        Parameters(params): Parameters<WorkItemUnlinkParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_unlink",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_unlink(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Report dependency cycles in the schedule graph (depends_on + blocks). Each cycle is a \
strongly-connected component of size > 1; an empty report (is_dag=true) means the schedule is a valid DAG. \
USE WHEN diagnosing why items are stuck or after a bulk import that bypassed the per-edge cycle guard."
    )]
    async fn work_item_cycles(
        &self,
        Parameters(params): Parameters<WorkItemCyclesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_cycles",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_cycles(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Anchor a work item to a code location (a file path and/or an explicit chunk_id/symbol_id; \
at least one must resolve). USE WHEN tying a task/clause to the precise code it concerns — feeds the auditor \
and change-impact surfaces."
    )]
    async fn work_item_anchor_code(
        &self,
        Parameters(params): Parameters<WorkItemAnchorCodeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_anchor_code",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_anchor_code(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Link a work item to a commit / PR / branch (the #<public_id> convention, made \
explicit). Pass ref_value (a commit SHA / PR number / branch name); link_type is inferred from its shape \
when omitted. For a commit, the SHA is resolved to an indexed git_commits row when available. This is a \
LINK only — it does NOT change status (repo activity advances items via the indexer's agent-grade \
auto-transition, and →verified still needs CI evidence). Idempotent on re-link."
    )]
    async fn work_item_link_commit(
        &self,
        Parameters(params): Parameters<WorkItemLinkCommitParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_link_commit",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_link_commit(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Burndown/velocity for a plan: verified-vs-remaining counts, realized velocity \
(items verified/day over the window), and a naive ETA. USE WHEN reporting plan progress or estimating \
completion. Reads the append-only status history — reflects evidence-verified completion, not agent claims."
    )]
    async fn work_item_burndown(
        &self,
        Parameters(params): Parameters<WorkItemBurndownParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_burndown",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_burndown(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Export a plan subtree as a markdown task list or an Org-mode outline (status → \
checkbox/keyword). USE WHEN sharing or archiving a plan as a portable document. Returns the rendered text."
    )]
    async fn work_item_export(
        &self,
        Parameters(params): Parameters<WorkItemExportParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_export",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_export(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Link a scientific experiment to the tracker as a kind='experiment' task (auto-created \
if no work_item_public_id is given) and seed an 'experiment_verdict' criterion. The experiment then gains \
priority/tags/progress/roll-up/claiming, and experiment_decide posts its statistical verdict as trusted \
evidence that auto-verifies the task. USE WHEN you want an experiment tracked + verified like any other task."
    )]
    async fn work_item_link_experiment(
        &self,
        Parameters(params): Parameters<WorkItemLinkExperimentParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_link_experiment",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_link_experiment(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "List one of the five built-in smart-view queues: my-work | needs-triage | overdue | \
blocked | next-actionable. my-work scopes to YOUR durable assignee (auto-filled from the MCP client name). \
USE WHEN you want a focused worklist instead of an unfiltered work_item_list. Read-only. Returns \
{view, count, items}."
    )]
    async fn work_item_view(
        &self,
        Parameters(mut params): Parameters<WorkItemViewParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // my-work scopes to the caller; fill the assignee from the client name
        // when omitted (mirrors how claim fills agent_id).
        if params.assignee.is_none() {
            params.assignee = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "work_item_view",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_view(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "The read-only 'what can I do now' frontier: actionable-status items (pending/confirmed/\
ready) whose every blocker is cleared, ranked like claim_next but WITHOUT claiming. Optionally scoped to a \
plan subtree and/or a durable assignee. USE WHEN deciding what to pick up next. Returns {count, actionable}."
    )]
    async fn work_item_next_actionable(
        &self,
        Parameters(params): Parameters<WorkItemNextActionableParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_next_actionable",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_next_actionable(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Set (or clear) an item's DURABLE assignee — durable ownership intent (1:1, never \
auto-expires, surfaced by the my-work view), ORTHOGONAL to the ephemeral claimed_by execution lease taken by \
work_item_claim. Assignment is NOT a status transition. Omit/empty assignee to UNASSIGN. USE WHEN recording \
who owns an item long-term (vs who is actively working it right now)."
    )]
    async fn work_item_assign(
        &self,
        Parameters(mut params): Parameters<WorkItemAssignParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.assigned_by.is_none() {
            params.assigned_by = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "work_item_assign",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_assign(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "The full per-item unified timeline: a chronological merge of status transitions, \
progress notes, claim/handoff events, verification evidence, and scope negotiations. Read-only. USE WHEN \
auditing an item's history — the auto-unblock cascade appears here as an actor_kind='system' blocked→ready \
event. Returns {public_id, events, timeline}."
    )]
    async fn work_item_history(
        &self,
        Parameters(params): Parameters<WorkItemHistoryParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_history",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_history(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Apply one operation to many items at once: set_status | tag | untag | reprioritize | \
assign. Select targets by explicit public_ids OR a smart-view (capped at 500). set_status loops through the \
per-item transition chokepoint as Actor::Agent — so each item gets full transition-legality (an illegal \
move lands in `failed`) AND the auto-unblock cascade. Partial-success: returns \
{op, attempted, succeeded, failed:[{public_id, error}]}."
    )]
    async fn work_item_bulk(
        &self,
        Parameters(params): Parameters<WorkItemBulkParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_bulk",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_bulk(self.ctx(), params),
        )
        .await
    }
}
