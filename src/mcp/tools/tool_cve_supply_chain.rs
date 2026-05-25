//! `tool_cve_supply_chain` — Parse Cargo.lock / package-lock.json /
//! requirements.txt and surface dependencies for OSV.dev review (SOTA Phase 6.7).

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::CveSupplyChainParams;
use crate::mcp::tools::sema_helpers::effects::symbols_with_any_effect;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::parsing::type_tags::vocabulary::{EFFECT_CRYPTO_WEAK, EFFECT_NETWORK, EFFECT_UNSAFE};
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
    // Shadow-ASR channel: symbols carrying effects that amplify CVE
    // blast-radius (unsafe / network / weak-crypto).
    let risky_effect_symbols = symbols_with_any_effect(
        pool,
        project_id,
        &[
            EFFECT_UNSAFE.to_string(),
            EFFECT_NETWORK.to_string(),
            EFFECT_CRYPTO_WEAK.to_string(),
        ],
    )
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(symbol_id, file_id, name, scope_path)| {
        serde_json::json!({
            "symbol_id": symbol_id, "file_id": file_id, "name": name, "scope_path": scope_path,
        })
    })
    .collect::<Vec<_>>();

    // Phase 4.5: match the inventory against OFFLINE-imported advisories
    // (`pgmcp import-advisories`). Local-only — no network. Group by ecosystem,
    // load advisories for the inventory's packages, SemVer-match each dependency.
    use std::collections::{HashMap, HashSet};
    let mut by_eco: HashMap<String, HashSet<String>> = HashMap::new();
    for d in &dependencies {
        if let (Some(eco), Some(name)) = (
            d.get("ecosystem").and_then(|v| v.as_str()),
            d.get("name").and_then(|v| v.as_str()),
        ) {
            by_eco
                .entry(eco.to_string())
                .or_default()
                .insert(name.to_string());
        }
    }
    let mut adv_map: HashMap<(String, String), Vec<crate::db::queries::VulnAdvisoryRow>> =
        HashMap::new();
    for (eco, pkgs) in &by_eco {
        let pkg_vec: Vec<String> = pkgs.iter().cloned().collect();
        if let Ok(rows) = crate::db::queries::load_vuln_advisories(pool, eco, &pkg_vec).await {
            for r in rows {
                adv_map
                    .entry((r.ecosystem.clone(), r.package.clone()))
                    .or_default()
                    .push(r);
            }
        }
    }
    let advisories_loaded: usize = adv_map.values().map(|v| v.len()).sum();
    let mut vulnerabilities: Vec<serde_json::Value> = Vec::new();
    for d in &dependencies {
        let (Some(eco), Some(name), Some(ver)) = (
            d.get("ecosystem").and_then(|v| v.as_str()),
            d.get("name").and_then(|v| v.as_str()),
            d.get("version").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let Some(version) = crate::code_analysis::vuln_match::parse_version(ver) else {
            continue;
        };
        if let Some(rows) = adv_map.get(&(eco.to_string(), name.to_string())) {
            for r in rows {
                let range = crate::code_analysis::vuln_match::VulnRange {
                    introduced: r.introduced.clone(),
                    fixed: r.fixed.clone(),
                    last_affected: r.last_affected.clone(),
                };
                if range.contains(&version) {
                    vulnerabilities.push(json!({
                        "advisory_id": r.advisory_id,
                        "ecosystem": eco,
                        "package": name,
                        "version": ver,
                        "severity": r.severity,
                        "summary": r.summary,
                        "introduced": r.introduced,
                        "fixed": r.fixed,
                    }));
                }
            }
        }
    }

    json_result(&json!({
        "project": params.project,
        "dependencies": dependencies,
        "advisories_loaded": advisories_loaded,
        "vulnerability_count": vulnerabilities.len(),
        "vulnerabilities": vulnerabilities,
        "risky_effect_symbols": risky_effect_symbols,
        "guidance": "`vulnerabilities` are dependency versions that fall in a known advisory's vulnerable \
            SemVer range, matched OFFLINE against advisories imported via `pgmcp import-advisories <osv-dump>` \
            (no network). `advisories_loaded=0` means no dump has been imported yet — run that command on a \
            local OSV/GHSA export first. Cross-reference flagged packages with `risky_effect_symbols` \
            (unsafe / network / weak-crypto) to gauge blast radius; `fixed` is the first non-vulnerable \
            version to upgrade to."
    }))
}
