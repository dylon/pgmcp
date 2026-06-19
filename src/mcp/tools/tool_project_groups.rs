//! `project_groups` — derive + list the project-grouping model (ADR-027, item 15).
//! Worktree families + singletons, with each group's members and their roles.

use std::collections::BTreeMap;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::hierarchy::GroupKind;
use crate::mcp::server::ProjectGroupsParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_project_groups(
    ctx: &SystemContext,
    params: ProjectGroupsParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    if let Some(k) = &params.kind
        && GroupKind::parse(k).is_none()
    {
        return Err(McpError::invalid_params(
            "invalid kind (worktree_family|monorepo|declared|manual)",
            None,
        ));
    }
    if params.rederive.unwrap_or(true) {
        crate::hierarchy::grouping::rederive_groups(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("rederive groups: {e}"), None))?;
    }

    let rows = sqlx::query_as::<_, (i64, String, String, Option<String>, i32, String, String)>(
        "SELECT g.id, g.kind, g.group_key, g.label, m.project_id, m.role, p.name
           FROM project_groups g
           JOIN project_group_members m ON m.group_id = g.id AND m.valid_to IS NULL
           JOIN projects p ON p.id = m.project_id
          WHERE ($1::text IS NULL OR g.kind = $1)
          ORDER BY g.id, (m.role = 'main') DESC, p.name",
    )
    .bind(params.kind.as_deref())
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("list groups: {e}"), None))?;

    // Assemble groups (BTreeMap keeps group id order stable).
    let mut groups: BTreeMap<i64, serde_json::Value> = BTreeMap::new();
    for (gid, kind, key, label, pid, role, pname) in rows {
        let entry = groups.entry(gid).or_insert_with(|| {
            json!({"group_id": gid, "kind": kind, "group_key": key, "label": label, "members": []})
        });
        entry["members"].as_array_mut().unwrap().push(json!({
            "project_id": pid, "project": pname, "role": role,
        }));
    }
    let list: Vec<_> = groups.into_values().collect();
    // A group with >1 member is a real worktree family / monorepo (not a singleton).
    let multi = list
        .iter()
        .filter(|g| {
            g["members"]
                .as_array()
                .map(|m| m.len() > 1)
                .unwrap_or(false)
        })
        .count();
    json_result(&json!({
        "count": list.len(),
        "multi_member_groups": multi,
        "groups": list,
    }))
}
