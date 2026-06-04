//! `tool_client_project_matrix` — MCP tool body: the m:n client↔project
//! attribution matrix (edit-weighted, recency-ordered) aggregated from
//! `client_file_events`, with a per-project recently-edited-files drill-down.

#![allow(unused_imports)]

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;

pub async fn tool_client_project_matrix(
    ctx: &SystemContext,
    params: ClientProjectMatrixParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    debug!(tool = "client_project_matrix", "MCP tool invoked");

    let Some(pool) = ctx.db().pool() else {
        return Err(McpError::internal_error(
            "database pool unavailable".to_string(),
            None,
        ));
    };

    let since = params.since_minutes.unwrap_or(1440).clamp(1, 44_640);
    let top_files = params.top_files_per_project.unwrap_or(5).clamp(0, 50) as usize;

    let rows = crate::db::queries::client_project_matrix(pool, since, params.project.as_deref())
        .await
        .map_err(|e| {
            McpError::internal_error(format!("client_project_matrix query failed: {e}"), None)
        })?;

    let files = crate::db::queries::recent_edited_files(pool, since, params.project.as_deref())
        .await
        .map_err(|e| {
            McpError::internal_error(format!("recent_edited_files query failed: {e}"), None)
        })?;

    // Top-N recently-edited files per project (rows already edits-desc ordered).
    let mut files_by_project: BTreeMap<Option<i32>, Vec<serde_json::Value>> = BTreeMap::new();
    for f in &files {
        let entry = files_by_project.entry(f.project_id).or_default();
        if entry.len() < top_files {
            entry.push(json!({ "path": f.abs_path, "edits": f.edits, "last_ts": f.last_ts }));
        }
    }

    // Group (client, project) rows by project (rows ordered by project_id).
    let mut groups: Vec<(Option<i32>, Option<String>, Vec<serde_json::Value>)> = Vec::new();
    for r in &rows {
        let client = json!({
            "client_name": r.client_name,
            "client_key": r.client_key,
            "pid": r.pid,
            "edit_count": r.edit_count,
            "read_count": r.read_count,
            "file_count": r.file_count,
            "last_edit": r.last_edit,
            "last_activity": r.last_activity,
        });
        match groups.last_mut() {
            Some((pid, _, list)) if *pid == r.project_id => list.push(client),
            _ => groups.push((r.project_id, r.project.clone(), vec![client])),
        }
    }

    let by_project: Vec<serde_json::Value> = groups
        .into_iter()
        .map(|(project_id, project, clients)| {
            json!({
                "project": project,
                "project_id": project_id,
                "client_count": clients.len(),
                "clients": clients,
                "recent_files": files_by_project.get(&project_id).cloned().unwrap_or_default(),
            })
        })
        .collect();

    let envelope = json!({
        "window_minutes": since,
        "project_count": by_project.len(),
        "by_project": by_project,
    });

    let body = serde_json::to_string_pretty(&envelope)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {e}"), None))?;

    debug!(
        tool = "client_project_matrix",
        window_minutes = since,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(body)]))
}
