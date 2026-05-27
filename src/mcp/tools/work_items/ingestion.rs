//! Plan-ingestion tools: auto-translate an agent's markdown plan into a tracked
//! `work_items` subtree (`work_item_ingest_plan`), and promote a discovered code
//! marker (TODO/FIXME/…) into a tracked item (`work_item_promote_marker`).
//!
//! Both produce **stable** `public_id`s (a slug + a hash of the parent-path /
//! marker location) so re-ingesting an edited plan / re-promoting the same
//! marker is idempotent and never resets work progress (see
//! `queries::upsert_ingested_item`).

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::context::SystemContext;
use crate::db::queries::{self, NewWorkItem};
use crate::mcp::server::{WorkItemIngestPlanParams, WorkItemPromoteMarkerParams};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::work_items::crud::map_db_err;
use crate::mcp::tools::work_items::slugify;
use crate::tracker::ingest::parse_plan;
use crate::tracker::kind::WorkItemKind;

/// First 8 hex chars of sha256 — a stable short suffix for idempotent ids.
fn short_hash(s: &str) -> String {
    let digest = Sha256::digest(s.as_bytes());
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// `work_item_ingest_plan` — parse a markdown plan into a tracked subtree.
/// Idempotent: re-ingesting an edited plan upserts by stable `public_id`,
/// refreshing structure but preserving each item's status/progress.
pub async fn tool_work_item_ingest_plan(
    ctx: &SystemContext,
    params: WorkItemIngestPlanParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let out = ingest_plan_core(
        ctx,
        &params.plan_markdown,
        params.project.as_deref(),
        params.definition_slug.as_deref(),
    )
    .await?;
    json_result(&out)
}

/// Shared ingestion core (used by the MCP tool and the REST `ingest_plan`
/// handler), returning the result JSON. `project` is a project name.
pub async fn ingest_plan_core(
    ctx: &SystemContext,
    plan_markdown: &str,
    project: Option<&str>,
    definition_slug: Option<&str>,
) -> Result<Value, McpError> {
    ctx.stats().plan_ingests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    if plan_markdown.trim().is_empty() {
        return Err(McpError::invalid_params(
            "plan_markdown must be non-empty",
            None,
        ));
    }
    let nodes = parse_plan(plan_markdown);
    if nodes.is_empty() {
        return Err(McpError::invalid_params(
            "no recognizable plan structure (need headings / checklists / numbered items)",
            None,
        ));
    }

    let project_id = queries::resolve_project_id(pool, project)
        .await
        .map_err(map_db_err)?;
    let definition_id = match definition_slug {
        None => None,
        Some(slug) => queries::get_plan_definition(pool, &slugify(slug), None)
            .await
            .map_err(map_db_err)?
            .map(|d| d.id),
    };

    let root_ns = slugify(&nodes[0].title);
    let mut ids: Vec<i64> = Vec::with_capacity(nodes.len());
    let mut pubs: Vec<String> = Vec::with_capacity(nodes.len());
    let mut created = 0_usize;

    for node in &nodes {
        let parent_id = node.parent_index.map(|pi| ids[pi]);
        let parent_pub = node
            .parent_index
            .map(|pi| pubs[pi].as_str())
            .unwrap_or(root_ns.as_str());
        let public_id = format!(
            "{}-{}",
            slugify(&node.title),
            short_hash(&format!("{parent_pub}/{}", node.title))
        );
        let status = if node.seed_claimed_done {
            "claimed_done"
        } else {
            "pending"
        };
        let (id, inserted) = queries::upsert_ingested_item(
            pool,
            &public_id,
            parent_id,
            project_id,
            definition_id,
            node.kind.as_str(),
            status,
            &node.title,
            node.body.as_deref(),
            node.parametric,
            node.parametric_corpus.as_deref(),
        )
        .await
        .map_err(map_db_err)?;
        if inserted {
            created += 1;
            let coverage_mode = if node.parametric {
                "universal"
            } else {
                "single"
            };
            for acc in &node.acceptance {
                queries::insert_acceptance_criterion(
                    pool,
                    id,
                    &acc.criterion_kind,
                    &acc.description,
                    acc.acceptance_uri.as_deref(),
                    Some(0),
                    coverage_mode,
                    None,
                    true,
                )
                .await
                .map_err(map_db_err)?;
            }
        }
        ids.push(id);
        pubs.push(public_id);
    }

    let mut out = json!({
        "root_public_id": pubs[0],
        "root_id": ids[0],
        "nodes": nodes.len(),
        "created": created,
        "updated": nodes.len() - created,
    });
    if let Some(def_id) = definition_id {
        let v = queries::validate_plan(pool, ids[0], def_id)
            .await
            .map_err(map_db_err)?;
        let errors = v.iter().filter(|x| x.severity == "error").count();
        out["validation"] = json!({
            "definition": definition_slug,
            "valid": errors == 0,
            "violations": v,
        });
    }
    Ok(out)
}

/// `work_item_promote_marker` — turn a discovered code marker (TODO/FIXME/…)
/// into a tracked item. Idempotent on the marker's text+location.
pub async fn tool_work_item_promote_marker(
    ctx: &SystemContext,
    params: WorkItemPromoteMarkerParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let text = params.marker_text.trim();
    if text.is_empty() {
        return Err(McpError::invalid_params(
            "marker_text must be non-empty",
            None,
        ));
    }

    let kind = match params.kind.as_deref() {
        Some(k) => WorkItemKind::parse(k)
            .ok_or_else(|| McpError::invalid_params(format!("invalid kind '{k}'"), None))?,
        None => {
            let upper = text.to_uppercase();
            if ["FIXME", "BUG", "HACK", "XXX", "KLUDGE", "WTF"]
                .iter()
                .any(|m| upper.contains(m))
            {
                WorkItemKind::Fixme
            } else {
                WorkItemKind::Todo
            }
        }
    };

    let project_id = queries::resolve_project_id(pool, params.project.as_deref())
        .await
        .map_err(map_db_err)?;
    let location = match (&params.file, params.line) {
        (Some(f), Some(l)) => Some(format!("{f}:{l}")),
        (Some(f), None) => Some(f.clone()),
        _ => None,
    };
    let title: String = text.chars().take(120).collect();
    let public_id = format!(
        "{}-{}",
        slugify(&title),
        short_hash(&format!(
            "marker/{}/{}",
            location.as_deref().unwrap_or(""),
            text
        ))
    );

    // Idempotent: a re-promote of the same marker returns the existing item.
    if let Some(existing) = queries::get_work_item_by_public_id(pool, &public_id)
        .await
        .map_err(map_db_err)?
    {
        return json_result(&json!({
            "public_id": existing.public_id,
            "id": existing.id,
            "kind": existing.kind,
            "already_promoted": true,
        }));
    }

    let body = location
        .as_ref()
        .map(|loc| format!("Promoted from code marker at {loc}\n\n{text}"));
    let item = NewWorkItem {
        public_id: &public_id,
        project_id,
        kind: kind.as_str(),
        title: &title,
        body: body.as_deref(),
        origin: "ingest_marker",
        ..Default::default()
    };
    let new_id = queries::insert_work_item(pool, item)
        .await
        .map_err(map_db_err)?;
    ctx.stats()
        .work_items_created
        .fetch_add(1, Ordering::Relaxed);
    let row = queries::get_work_item(pool, new_id)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| McpError::internal_error("promoted item vanished", None))?;
    json_result(&row)
}
