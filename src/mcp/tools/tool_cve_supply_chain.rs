//! `tool_cve_supply_chain` — Parse Cargo.lock / package-lock.json /
//! requirements.txt and surface dependencies for OSV.dev review (SOTA Phase 6.7).

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::CveSupplyChainParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use std::str::FromStr;

pub async fn tool_cve_supply_chain(
    ctx: &SystemContext,
    params: CveSupplyChainParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "cve_supply_chain", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    let manifests: Vec<(String, Option<String>)> = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT relative_path, content
         FROM indexed_files
         WHERE project_id = $1
           AND (relative_path LIKE '%Cargo.lock' OR relative_path LIKE '%package-lock.json'
                OR relative_path LIKE '%requirements.txt' OR relative_path LIKE '%pnpm-lock.yaml'
                OR relative_path LIKE '%go.sum' OR relative_path LIKE '%pom.xml')",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Manifest query failed: {}", e), None))?;

    let mut dependencies: Vec<serde_json::Value> = Vec::new();
    for (path, content) in manifests {
        let Some(c) = content else { continue };
        if path.ends_with("Cargo.lock") {
            if let Ok(lock) = <cargo_lock::Lockfile as FromStr>::from_str(&c) {
                for pkg in &lock.packages {
                    dependencies.push(json!({
                        "manifest": path,
                        "ecosystem": "crates.io",
                        "name": pkg.name.as_str(),
                        "version": pkg.version.to_string(),
                    }));
                }
            }
        } else if path.ends_with("requirements.txt") {
            for line in c.lines() {
                let line = line.split('#').next().unwrap_or("").trim();
                if line.is_empty() {
                    continue;
                }
                let (name, ver) = if let Some((n, v)) = line.split_once("==") {
                    (n.trim().to_string(), v.trim().to_string())
                } else if let Some((n, v)) = line.split_once(">=") {
                    (n.trim().to_string(), v.trim().to_string())
                } else {
                    (line.to_string(), String::new())
                };
                if !name.is_empty() {
                    dependencies.push(json!({
                        "manifest": path,
                        "ecosystem": "PyPI",
                        "name": name,
                        "version": ver,
                    }));
                }
            }
        } else if path.ends_with("package-lock.json") {
            // Parse JSON pkgs at top level + "packages" tree.
            if let Ok(j) = serde_json::from_str::<serde_json::Value>(&c)
                && let Some(obj) = j.get("packages").and_then(|p| p.as_object())
            {
                for (k, v) in obj.iter() {
                    if k.is_empty() {
                        continue;
                    }
                    let name = k.rsplit("node_modules/").next().unwrap_or(k);
                    let version = v.get("version").and_then(|x| x.as_str()).unwrap_or("");
                    dependencies.push(json!({
                        "manifest": path,
                        "ecosystem": "npm",
                        "name": name,
                        "version": version,
                    }));
                }
            }
        }
        // go.sum / pom.xml left for advanced ecosystem parsers; deps still
        // surfaced as path-only entries so the user knows to audit them.
        else {
            dependencies.push(json!({
                "manifest": path,
                "note": "Manifest detected but no parser configured; pass to a dedicated audit tool.",
            }));
        }
    }
    let limit = params.limit.unwrap_or(200);
    dependencies.truncate(limit.max(0) as usize);
    json_result(&json!({
        "project": params.project,
        "dependencies": dependencies,
        "guidance": "Lists every dependency parsed from lockfiles. Cross-reference with https://api.osv.dev/v1/querybatch (network access required) — pgmcp surfaces the inventory only, leaving the live CVE lookup to the operator's audit workflow."
    }))
}
