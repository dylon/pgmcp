//! Work-item / plan tracker handlers (part A: CRUD, tags, progress, plans).
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

#[rmcp::tool_router(router = router_work_items_a, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Create a work item (plan/goal/epic/task/sub_task/todo/fixme/idea/note/question/\
nice_to_have/action_item/experiment), optionally under a parent and scoped to a project. USE WHEN you need to \
record a tracked unit of work or decompose a plan into a hierarchy. DO NOT USE WHEN you just want a free-form \
note to yourself outside the tracker. Returns the created row (with its generated public_id)."
    )]
    async fn work_item_create(
        &self,
        Parameters(params): Parameters<WorkItemCreateParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_create",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_create(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Fetch one work item by its public_id, optionally with its full descendant subtree. USE \
WHEN you need the current state of a specific item (status, priority, parent, timestamps). DO NOT USE WHEN you \
want to browse/filter many items — use work_item_list instead. Returns {item, subtree?}."
    )]
    async fn work_item_get(
        &self,
        Parameters(params): Parameters<WorkItemGetParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_get",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_get(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Update a work item's mutable non-status fields (title, body, priority, weight) by \
public_id; omitted fields are left unchanged. USE WHEN re-grooming an item. DO NOT USE WHEN you want to change \
its lifecycle status — use work_item_set_status (status transitions are gated). Returns the updated row."
    )]
    async fn work_item_update(
        &self,
        Parameters(params): Parameters<WorkItemUpdateParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_update",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_update(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "List work items (newest/highest-priority first), filterable by project, kind, status, and \
parent public_id. USE WHEN browsing or triaging the backlog. DO NOT USE WHEN you already know the exact \
public_id — use work_item_get. Returns an array of rows."
    )]
    async fn work_item_list(
        &self,
        Parameters(params): Parameters<WorkItemListParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_list",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_list(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Return a work item and its entire descendant subtree (depth-ordered) by public_id. USE \
WHEN you need the materialized hierarchy under a plan/epic for roll-up or rendering. DO NOT USE WHEN you only \
need the single item — use work_item_get. Returns an array of rows ordered by depth then priority."
    )]
    async fn work_item_tree(
        &self,
        Parameters(params): Parameters<WorkItemTreeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_tree",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_tree(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Move a work item (and its subtree) under a new parent by public_id, or to the root \
(omit new_parent_public_id). USE WHEN re-organizing the hierarchy. DO NOT USE WHEN the target parent is the item \
itself or one of its own descendants — that is rejected to prevent a cycle. Returns the updated row."
    )]
    async fn work_item_reparent(
        &self,
        Parameters(params): Parameters<WorkItemReparentParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_reparent",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_reparent(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Transition a work item's lifecycle status by public_id, AS THE AGENT. USE WHEN advancing \
your own work (ready, in_progress, blocked, claimed_done, verifying, cancelled). DO NOT USE to mark work \
verified/deferred/rejected — the agent actor cannot reach those states (they require user negotiation or \
gatekeeper evidence); such a request is refused with an explanatory error. Returns the updated row."
    )]
    async fn work_item_set_status(
        &self,
        Parameters(params): Parameters<WorkItemSetStatusParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_set_status",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_set_status(self.ctx(), params),
        )
        .await
    }

    // ── Phase 2: tags + progress ────────────────────────────────────────────

    #[tool(
        description = "Create (or upsert) a shared tag in the catalog, addressed by a stable slug derived \
from the name. USE WHEN you want a reusable label to attach across many work items (e.g. 'urgent', 'tech-debt'). \
DO NOT USE WHEN you just want to attach an existing tag to one item — use work_item_tag. Re-running with the \
same name updates the color/description without clobbering existing values. Returns the tag row."
    )]
    async fn tag_create(
        &self,
        Parameters(params): Parameters<TagCreateParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tag_create",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_tag_create(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "List tags in the catalog, ordered by name. USE WHEN browsing the available labels or \
building a tag picker. DO NOT USE WHEN you want the tags ON a specific item — fetch the item (work_item_tag \
returns its current tags). By default returns active tags only; pass include_merged=true to also see \
tombstoned (merged) tags. Returns an array of tag rows."
    )]
    async fn tag_list(
        &self,
        Parameters(params): Parameters<TagListParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tag_list",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_tag_list(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Merge one tag into another: repoint every item tagged with src so it is tagged with \
dst instead, then tombstone src (its slug still resolves to dst). USE WHEN consolidating duplicate/synonym \
tags. DO NOT USE WHEN you merely want to rename a tag — use tag_rename (which keeps the slug stable). src/dst \
may be slugs or labels. Returns {merged: <count>, into: <dst_slug>}; an unknown tag is an invalid_params error."
    )]
    async fn tag_merge(
        &self,
        Parameters(params): Parameters<TagMergeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tag_merge",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_tag_merge(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Rename a tag in place by slug; the slug is intentionally preserved so existing \
references survive. USE WHEN fixing a label's display name. DO NOT USE WHEN you want to fold two tags together \
— use tag_merge. The lookup key is slugified, so you may pass either the slug or the original label. Returns \
the updated tag row; a missing tag is an invalid_params error."
    )]
    async fn tag_rename(
        &self,
        Parameters(params): Parameters<TagRenameParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tag_rename",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_tag_rename(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Attach one or more tags to a work item (by public_id), auto-creating unknown tags by \
default. USE WHEN labeling an item for triage/filtering. DO NOT USE WHEN you want to define a tag's \
metadata (color/description) — use tag_create. With auto_create=false, unknown tags are returned under \
'skipped' instead of being created. Returns {item, applied:[slugs], skipped:[names], tags:[current tags]}."
    )]
    async fn work_item_tag(
        &self,
        Parameters(params): Parameters<WorkItemTagParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_tag",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_tag(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Detach a single tag from a work item (by public_id). USE WHEN a label no longer \
applies. DO NOT USE WHEN you want to delete the tag globally — untag only removes the item↔tag pairing, the \
catalog tag remains. The tag is slugified for lookup; an unknown tag is an invalid_params error. Returns \
{removed: <bool>} (false if the pairing did not exist)."
    )]
    async fn work_item_untag(
        &self,
        Parameters(params): Parameters<WorkItemUntagParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_untag",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_untag(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Append a progress note to a work item (by public_id), optionally with a self-reported \
percent that updates the item's claimed_percent. USE WHEN recording incremental progress / an activity-feed \
entry as you work. DO NOT USE to change the item's lifecycle status — use work_item_set_status. The note is \
recorded as provenance='agent_write' (the agent's claim, NOT trusted for the verified roll-up). Returns the \
new progress row."
    )]
    async fn work_item_record_progress(
        &self,
        Parameters(mut params): Parameters<WorkItemRecordProgressParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.agent_id.is_none() {
            params.agent_id = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "work_item_record_progress",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_record_progress(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Read a work item's progress log, newest first (by public_id). USE WHEN reviewing the \
activity history / how an item progressed over time. DO NOT USE WHEN you only need the current status or \
claimed_percent — use work_item_get. Returns an array of progress rows (note, percent, provenance, timestamps)."
    )]
    async fn work_item_progress_log(
        &self,
        Parameters(params): Parameters<WorkItemProgressLogParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_progress_log",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_progress_log(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Weighted completion roll-up of a work item's subtree. USE WHEN you need overall \
progress of a plan/epic/goal. Returns BOTH verified_* (trustworthy: only evidence-verified leaves count) and \
claimed_* (advisory: also counts agent-reported claimed_done). DO NOT treat claimed_* as actually done."
    )]
    async fn work_item_completion(
        &self,
        Parameters(params): Parameters<WorkItemCompletionParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_completion",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_completion(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Recompute computed_score for active items (recency × manual priority × \
dependency-unblock) and return a now/next/later work plan of the top items. USE WHEN deciding what to work \
on next across a backlog. DO NOT USE WHEN you just want a filtered list — use work_item_list."
    )]
    async fn work_item_reprioritize(
        &self,
        Parameters(params): Parameters<WorkItemReprioritizeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_reprioritize",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_reprioritize(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Semantic search over the work-item backlog by meaning (cosine over BGE-M3 \
embeddings). USE WHEN finding items related to a concept/topic across the tracker. DO NOT USE WHEN you have \
an exact public_id (use work_item_get) or want a structured filter (use work_item_list)."
    )]
    async fn work_item_search(
        &self,
        Parameters(params): Parameters<WorkItemSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Define a reusable plan template + its dictated structural rules (required kinds, \
allowed/required child kinds, min/max children, required fields, required acceptance criteria, \
quantifier-needs-corpus, naming/id regex, max-depth advice). Plan instances are checked against it with \
plan_validate. Re-defining a (slug, version) replaces its rule set."
    )]
    async fn plan_define(
        &self,
        Parameters(params): Parameters<PlanDefineParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "plan_define",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_plan_define(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Validate a plan instance (the subtree under root_public_id) against a plan definition's \
rules; returns a severity-sorted violations report (advisory — reports, does not block). USE WHEN checking a \
plan conforms to a template. DO NOT confuse with verification — that gates on evidence, not structure."
    )]
    async fn plan_validate(
        &self,
        Parameters(params): Parameters<PlanValidateParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "plan_validate",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_plan_validate(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Export a stored plan definition (metadata + [scope] passthrough + rules) to \
serene-eclipse-shaped TOML. Always returns the TOML string; if 'path' is given, also writes the file. USE \
WHEN producing a portable/inspectable .claude/tasks/<slug>.toml artifact. DB stays the source of truth."
    )]
    async fn plan_definition_export(
        &self,
        Parameters(params): Parameters<PlanDefinitionExportParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "plan_definition_export",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_plan_definition_export(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Import a serene-eclipse-shaped TOML definition ([definition] + optional [scope] + \
[[rule]]) into the tracker — inline via 'toml' or from a file via 'path'. Idempotent on (slug, version); \
replaces the rule set and stores the raw TOML in body_toml. USE WHEN loading a shared/edited plan template."
    )]
    async fn plan_definition_import(
        &self,
        Parameters(params): Parameters<PlanDefinitionImportParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "plan_definition_import",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_plan_definition_import(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Attach a machine-checkable acceptance criterion to an item (its definition-of-done). \
USE WHEN specifying what must pass for a task to be verifiable. Pair with record_evidence + attempt_verify."
    )]
    async fn work_item_add_criterion(
        &self,
        Parameters(params): Parameters<WorkItemAddCriterionParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "work_item_add_criterion",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::work_items::tool_work_item_add_criterion(self.ctx(), params),
        )
        .await
    }
}
