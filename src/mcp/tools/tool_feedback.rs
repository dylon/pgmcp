//! Agent-feedback + voting tool bodies (ADR-023). Thin DB-backed handlers over
//! `crate::db::queries::{feedback,votes}`, mirroring the data-table tools.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::db::queries;
use crate::feedback::{FeedbackCategory, FeedbackSentiment, FeedbackStatus};
// Param structs are re-exported at `crate::mcp::server::*` (server.rs `pub use
// params::*`), the same path every flat tool body uses.
use crate::mcp::server::{
    CastVoteParams, ListFeedbackParams, PromoteFeedbackParams, RespondFeedbackParams,
    RetractVoteParams, SearchFeedbackParams, SubmitFeedbackParams, TallyVotesParams,
};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::voting::{VoteDirection, VoteTargetType};

fn bad(msg: &str) -> McpError {
    McpError::invalid_params(msg.to_string(), None)
}

/// Resolve an optional project name to an id (error on a non-empty, unknown name).
async fn resolve_project_opt(
    ctx: &SystemContext,
    project: &Option<String>,
) -> Result<Option<i32>, McpError> {
    match project {
        Some(p) if !p.trim().is_empty() => Ok(Some(project_id_or_err(ctx, p).await?)),
        _ => Ok(None),
    }
}

fn agent_of(agent_id: &Option<String>) -> &str {
    agent_id.as_deref().unwrap_or("unknown-agent")
}

// ---------------------------------------------------------------- feedback ---

pub async fn tool_submit_feedback(
    ctx: &SystemContext,
    params: SubmitFeedbackParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let category = FeedbackCategory::parse(&params.category).ok_or_else(|| {
        bad("invalid category (complaint|feature_request|praise|bug_report|question|suggestion)")
    })?;
    let sentiment = FeedbackSentiment::parse(&params.sentiment).ok_or_else(|| {
        bad("invalid sentiment (strongly_negative|negative|neutral|positive|strongly_positive)")
    })?;
    if params.body.trim().is_empty() {
        return Err(bad("body must be non-empty"));
    }
    let project_id = resolve_project_opt(ctx, &params.project).await?;
    let agent = agent_of(&params.agent_id);

    let id = queries::insert_feedback(
        pool,
        queries::NewFeedback {
            agent_id: agent,
            category: category.as_str(),
            sentiment: sentiment.as_str(),
            subject: params.subject.as_deref(),
            body: &params.body,
            about_tool: params.about_tool.as_deref(),
            project_id,
        },
    )
    .await
    .map_err(|e| McpError::internal_error(format!("insert feedback: {e}"), None))?;

    // Embed-on-write (best-effort): subject + body.
    let mut text = params.subject.clone().unwrap_or_default();
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(&params.body);
    match ctx.embed().embed_query(&text).await {
        Ok(v) => {
            if let Err(e) =
                queries::set_feedback_embedding(pool, id, pgvector::Vector::from(v)).await
            {
                tracing::error!(error = %e, id, "feedback embed-on-write store failed; leaving NULL");
            }
        }
        Err(e) => {
            tracing::error!(error = %e, id, "feedback embed-on-write failed; leaving NULL")
        }
    }

    json_result(&json!({
        "id": id,
        "category": category.as_str(),
        "sentiment": sentiment.as_str(),
        "status": "open",
        "agent_id": agent,
    }))
}

pub async fn tool_list_feedback(
    ctx: &SystemContext,
    params: ListFeedbackParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    // Validate any provided enum filters so a typo fails loudly, not silently empty.
    if let Some(c) = &params.category
        && FeedbackCategory::parse(c).is_none()
    {
        return Err(bad("invalid category filter"));
    }
    if let Some(s) = &params.sentiment
        && FeedbackSentiment::parse(s).is_none()
    {
        return Err(bad("invalid sentiment filter"));
    }
    if let Some(s) = &params.status
        && FeedbackStatus::parse(s).is_none()
    {
        return Err(bad("invalid status filter"));
    }
    let project_id = resolve_project_opt(ctx, &params.project).await?;
    let limit = params.limit.unwrap_or(50).clamp(1, 500);
    let rows = queries::list_feedback(
        pool,
        params.category.as_deref(),
        params.sentiment.as_deref(),
        params.status.as_deref(),
        params.about_tool.as_deref(),
        project_id,
        limit,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("list feedback: {e}"), None))?;
    json_result(&json!({ "count": rows.len(), "feedback": rows }))
}

pub async fn tool_search_feedback(
    ctx: &SystemContext,
    params: SearchFeedbackParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    if params.query.trim().is_empty() {
        return Err(bad("query must be non-empty"));
    }
    let mode = params.mode.as_deref().unwrap_or("hybrid");
    let limit = params.limit.unwrap_or(20).clamp(1, 200);

    let mut rows = match mode {
        "fts" => queries::search_feedback_fts(pool, &params.query, limit)
            .await
            .map_err(|e| McpError::internal_error(format!("fts: {e}"), None))?,
        "semantic" | "hybrid" => {
            let mut out = if mode == "hybrid" {
                queries::search_feedback_fts(pool, &params.query, limit)
                    .await
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            // Semantic leg (best-effort embed).
            if let Ok(v) = ctx.embed().embed_query(&params.query).await {
                let sem = queries::search_feedback_semantic(pool, pgvector::Vector::from(v), limit)
                    .await
                    .map_err(|e| McpError::internal_error(format!("semantic: {e}"), None))?;
                // Union by id, preserving order (fts first, then new semantic hits).
                let mut seen: std::collections::HashSet<i64> = out.iter().map(|r| r.id).collect();
                for r in sem {
                    if seen.insert(r.id) {
                        out.push(r);
                    }
                }
            }
            out
        }
        other => {
            return Err(bad(&format!(
                "invalid mode '{other}' (fts|semantic|hybrid)"
            )));
        }
    };
    rows.truncate(limit as usize);
    json_result(&json!({ "mode": mode, "count": rows.len(), "feedback": rows }))
}

pub async fn tool_respond_feedback(
    ctx: &SystemContext,
    params: RespondFeedbackParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let status = FeedbackStatus::parse(&params.status)
        .ok_or_else(|| bad("invalid status (open|acknowledged|planned|resolved|declined)"))?;
    let updated = queries::respond_feedback(
        pool,
        params.id,
        status.as_str(),
        agent_of(&params.agent_id),
        params.response.as_deref(),
    )
    .await
    .map_err(|e| McpError::internal_error(format!("respond feedback: {e}"), None))?;
    if !updated {
        return Err(bad(&format!("no feedback with id {}", params.id)));
    }
    json_result(&json!({ "id": params.id, "status": status.as_str(), "updated": true }))
}

pub async fn tool_promote_feedback_to_work_item(
    ctx: &SystemContext,
    params: PromoteFeedbackParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let fb = queries::get_feedback(pool, params.id)
        .await
        .map_err(|e| McpError::internal_error(format!("get feedback: {e}"), None))?
        .ok_or_else(|| bad(&format!("no feedback with id {}", params.id)))?;

    // Idempotent: if already promoted, return the existing work-item.
    if let Some(existing) = fb.promoted_work_item_id {
        return json_result(&json!({
            "feedback_id": fb.id, "work_item_public_id": format!("feedback-{}", fb.id),
            "work_item_id": existing, "already_promoted": true,
        }));
    }

    let title = params
        .title
        .clone()
        .or_else(|| fb.subject.clone())
        .unwrap_or_else(|| {
            let t: String = fb.body.chars().take(72).collect();
            t
        });
    let public_id = format!("feedback-{}", fb.id);
    let wid = queries::insert_work_item(
        pool,
        queries::NewWorkItem {
            public_id: &public_id,
            parent_id: None,
            project_id: fb.project_id,
            definition_id: None,
            kind: "task",
            status: "pending",
            title: &title,
            body: Some(&fb.body),
            priority: 0,
            weight: 1.0,
            parametric: false,
            parametric_corpus: None,
            parametric_expected: None,
            origin: "user_explicit",
            created_by: params.agent_id.as_deref(),
            severity: None,
            embedding: None,
        },
    )
    .await
    .map_err(|e| McpError::internal_error(format!("create work item: {e}"), None))?;

    queries::mark_feedback_promoted(pool, fb.id, wid)
        .await
        .map_err(|e| McpError::internal_error(format!("mark promoted: {e}"), None))?;

    json_result(&json!({
        "feedback_id": fb.id, "work_item_public_id": public_id,
        "work_item_id": wid, "already_promoted": false,
    }))
}

// ------------------------------------------------------------------- votes ---

fn validate_vote_target(target_type: &str) -> Result<VoteTargetType, McpError> {
    VoteTargetType::parse(target_type)
        .ok_or_else(|| bad("invalid target_type (work_item|feedback|bug|experiment)"))
}

pub async fn tool_cast_vote(
    ctx: &SystemContext,
    params: CastVoteParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let target = validate_vote_target(&params.target_type)?;
    let direction = VoteDirection::parse(&params.direction)
        .ok_or_else(|| bad("invalid direction (up|down)"))?;
    let weight = params.weight.unwrap_or(1.0);
    if weight <= 0.0 || !weight.is_finite() {
        return Err(bad("weight must be > 0"));
    }
    let agent = agent_of(&params.agent_id);
    let id = queries::cast_vote(
        pool,
        target.as_str(),
        params.target_id,
        agent,
        direction.as_str(),
        weight,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("cast vote: {e}"), None))?;
    let tally = queries::tally_votes(pool, target.as_str(), params.target_id)
        .await
        .map_err(|e| McpError::internal_error(format!("tally: {e}"), None))?;
    json_result(&json!({
        "vote_id": id, "target_type": target.as_str(), "target_id": params.target_id,
        "direction": direction.as_str(), "agent_id": agent, "tally": tally,
    }))
}

pub async fn tool_retract_vote(
    ctx: &SystemContext,
    params: RetractVoteParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let target = validate_vote_target(&params.target_type)?;
    let removed = queries::retract_vote(
        pool,
        target.as_str(),
        params.target_id,
        agent_of(&params.agent_id),
    )
    .await
    .map_err(|e| McpError::internal_error(format!("retract vote: {e}"), None))?;
    json_result(&json!({
        "target_type": target.as_str(), "target_id": params.target_id, "removed": removed,
    }))
}

pub async fn tool_tally_votes(
    ctx: &SystemContext,
    params: TallyVotesParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let target = validate_vote_target(&params.target_type)?;
    let tally = queries::tally_votes(pool, target.as_str(), params.target_id)
        .await
        .map_err(|e| McpError::internal_error(format!("tally: {e}"), None))?;
    json_result(&json!({
        "target_type": target.as_str(), "target_id": params.target_id, "tally": tally,
    }))
}
