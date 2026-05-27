//! Tag tool bodies for the work-item tracker: create/list/merge/rename a tag,
//! and attach/detach tags to/from an item. The catalog is the shared `tags`
//! table; the many-to-many join is `work_item_tags` (see
//! `crate::db::migrations::v4_work_items`). Tags are addressed by a stable
//! `slug` derived with the subsystem's [`slugify`](super::slugify) rule, so the
//! same human label always resolves to the same slug regardless of casing or
//! punctuation.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::db::queries::{
    get_tag_by_slug, list_item_tags, list_tags, merge_tags, rename_tag, tag_work_item,
    untag_work_item, upsert_tag,
};
use crate::mcp::server::{
    TagCreateParams, TagListParams, TagMergeParams, TagRenameParams, WorkItemTagParams,
    WorkItemUntagParams,
};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::work_items::crud::{id_of_public, map_db_err};
use crate::mcp::tools::work_items::slugify;

// ============================================================================
// tag_create
// ============================================================================

pub async fn tool_tag_create(
    ctx: &SystemContext,
    params: TagCreateParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let name = params.name.trim();
    if name.is_empty() {
        return Err(McpError::invalid_params("tag name must be non-empty", None));
    }
    let slug = slugify(name);

    let id = upsert_tag(
        pool,
        name,
        &slug,
        params.color.as_deref(),
        params.description.as_deref(),
    )
    .await
    .map_err(map_db_err)?;

    let row = get_tag_by_slug(pool, &slug)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| McpError::internal_error("upserted tag vanished", None))?;
    debug_assert_eq!(row.id, id);
    json_result(&row)
}

// ============================================================================
// tag_list
// ============================================================================

pub async fn tool_tag_list(
    ctx: &SystemContext,
    params: TagListParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let rows = list_tags(pool, params.include_merged.unwrap_or(false))
        .await
        .map_err(map_db_err)?;
    json_result(&rows)
}

// ============================================================================
// tag_merge
// ============================================================================

pub async fn tool_tag_merge(
    ctx: &SystemContext,
    params: TagMergeParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    // Both ends may be supplied as a slug OR a free label; slugify normalizes.
    let src_slug = slugify(&params.src);
    let dst_slug = slugify(&params.dst);

    let merged = merge_tags(pool, &src_slug, &dst_slug)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => McpError::invalid_params("unknown tag", None),
            other => map_db_err(other),
        })?;

    json_result(&json!({ "merged": merged, "into": dst_slug }))
}

// ============================================================================
// tag_rename
// ============================================================================

pub async fn tool_tag_rename(
    ctx: &SystemContext,
    params: TagRenameParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let new_name = params.new_name.trim();
    if new_name.is_empty() {
        return Err(McpError::invalid_params("new_name must be non-empty", None));
    }
    // The lookup key is a slug; slugify the supplied value so a caller may pass
    // either the slug itself or the original label.
    let slug = slugify(&params.slug);

    let row = rename_tag(pool, &slug, new_name)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| McpError::invalid_params(format!("no tag '{slug}'"), None))?;
    json_result(&row)
}

// ============================================================================
// work_item_tag
// ============================================================================

pub async fn tool_work_item_tag(
    ctx: &SystemContext,
    params: WorkItemTagParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let item_id = id_of_public(pool, &params.public_id).await?;
    let auto_create = params.auto_create.unwrap_or(true);

    let mut applied: Vec<String> = Vec::with_capacity(params.tags.len());
    let mut skipped: Vec<String> = Vec::new();

    for raw in &params.tags {
        let name = raw.trim();
        if name.is_empty() {
            continue;
        }
        let slug = slugify(name);

        // Resolve the tag, auto-creating it when permitted.
        let tag = match get_tag_by_slug(pool, &slug).await.map_err(map_db_err)? {
            Some(t) => t,
            None if auto_create => {
                upsert_tag(pool, name, &slug, None, None)
                    .await
                    .map_err(map_db_err)?;
                get_tag_by_slug(pool, &slug)
                    .await
                    .map_err(map_db_err)?
                    .ok_or_else(|| McpError::internal_error("auto-created tag vanished", None))?
            }
            None => {
                skipped.push(name.to_string());
                continue;
            }
        };

        tag_work_item(pool, item_id, tag.id, None)
            .await
            .map_err(map_db_err)?;
        applied.push(tag.slug);
    }

    ctx.stats()
        .work_item_tags_applied
        .fetch_add(applied.len() as u64, Ordering::Relaxed);

    let item_tags = list_item_tags(pool, item_id).await.map_err(map_db_err)?;
    json_result(&json!({
        "item": params.public_id,
        "applied": applied,
        "skipped": skipped,
        "tags": item_tags,
    }))
}

// ============================================================================
// work_item_untag
// ============================================================================

pub async fn tool_work_item_untag(
    ctx: &SystemContext,
    params: WorkItemUntagParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let item_id = id_of_public(pool, &params.public_id).await?;
    let slug = slugify(&params.tag);
    let tag = get_tag_by_slug(pool, &slug)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| McpError::invalid_params(format!("unknown tag '{slug}'"), None))?;

    let removed = untag_work_item(pool, item_id, tag.id)
        .await
        .map_err(map_db_err)?;
    json_result(&json!({ "removed": removed > 0 }))
}
