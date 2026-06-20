//! `cross_project_cve_exposure` (ADR-027 E6): propagate supply-chain CVE
//! exposure across the project dependency graph. A vulnerable dependency in
//! project D exposes every transitive DEPENDENT of D; inherited severity is
//! decayed by the minimum edge-confidence along the path. Direct exposure comes
//! from `hierarchy::security::direct_exposure` (manifest package names matched
//! against `vuln_advisories`).

use std::collections::{HashMap, HashSet};

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::hierarchy::security::direct_exposure;
use crate::mcp::server::CrossProjectCveExposureParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

/// One project's exposure row: (project_id, direct vulnerable packages, direct
/// severity rank, inherited severity, inherited-via paths).
type ExposureRow = (i32, Vec<String>, i32, f64, Vec<serde_json::Value>);

pub async fn tool_cross_project_cve_exposure(
    ctx: &SystemContext,
    params: CrossProjectCveExposureParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(100).clamp(1, 1000) as usize;

    let direct = direct_exposure(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("direct exposure: {e}"), None))?;

    let names: HashMap<i32, String> =
        sqlx::query_as::<_, (i32, String)>("SELECT id, name FROM projects")
            .fetch_all(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("projects: {e}"), None))?
            .into_iter()
            .collect();

    // dependent → [(dependency, confidence)] — forward edges for reachability.
    let edges: Vec<(i32, i32, f64)> = sqlx::query_as(
        "SELECT dependent_project_id, dependency_project_id, MAX(confidence)
           FROM project_dependencies
          WHERE valid_to IS NULL AND dependent_project_id <> dependency_project_id
          GROUP BY dependent_project_id, dependency_project_id",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("edges: {e}"), None))?;
    let mut adj: HashMap<i32, Vec<(i32, f64)>> = HashMap::new();
    for (dependent, dependency, conf) in &edges {
        adj.entry(*dependent)
            .or_default()
            .push((*dependency, *conf));
    }

    // For each project, walk its transitive dependencies; if a reached project
    // has direct exposure, it contributes severity × min-confidence-along-path.
    let mut results: Vec<ExposureRow> = Vec::new();
    for (&p, _) in names.iter() {
        let (direct_pkgs, direct_sev) = direct.get(&p).cloned().unwrap_or((Vec::new(), 0));
        let mut inherited_sev = 0.0f64;
        let mut via: Vec<serde_json::Value> = Vec::new();
        let mut stack = vec![(p, 1.0f64, 0usize)];
        let mut seen: HashSet<i32> = HashSet::new();
        while let Some((cur, conf, depth)) = stack.pop() {
            if depth > 12 || !seen.insert(cur) {
                continue;
            }
            if cur != p
                && let Some((pkgs, sev)) = direct.get(&cur)
            {
                let contrib = *sev as f64 * conf;
                if contrib > inherited_sev {
                    inherited_sev = contrib;
                }
                via.push(json!({
                    "via_project": names.get(&cur),
                    "severity_rank": sev,
                    "min_confidence": conf,
                    "vulnerable_packages": pkgs,
                }));
            }
            if let Some(neis) = adj.get(&cur) {
                for (n, c) in neis {
                    stack.push((*n, conf.min(*c), depth + 1));
                }
            }
        }
        if !direct_pkgs.is_empty() || inherited_sev > 0.0 {
            results.push((p, direct_pkgs, direct_sev, inherited_sev, via));
        }
    }

    // Highest total exposure (direct + inherited) first.
    results.sort_by(|a, b| {
        (b.2 as f64 + b.3)
            .partial_cmp(&(a.2 as f64 + a.3))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let exposed: Vec<_> = results
        .into_iter()
        .take(limit)
        .map(|(p, pkgs, dsev, isev, via)| {
            json!({
                "project": names.get(&p),
                "direct_severity_rank": dsev,
                "direct_vulnerable_packages": pkgs,
                "inherited_severity": isev,
                "total_exposure": dsev as f64 + isev,
                "exposed_via": via,
            })
        })
        .collect();

    json_result(&json!({
        "exposed_project_count": exposed.len(),
        "projects": exposed,
        "note": "Direct exposure = manifest package names matched against imported advisories \
    (run `pgmcp import-advisories`). Inherited = a vulnerable dependency project propagated to its \
    transitive dependents, severity × min edge-confidence. Precise per-project SemVer matching is \
    `cve_supply_chain`.",
        "guidance": if exposed.is_empty() {
            Some("no exposure found — import an OSV dump (pgmcp import-advisories) and run the project-deps cron")
        } else { None },
    }))
}
