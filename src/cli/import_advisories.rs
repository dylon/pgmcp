//! `pgmcp import-advisories <path>` — offline OSV/GHSA dump import
//! (graph-roadmap Phase 4.5).
//!
//! Reads a LOCAL OSV dump — a single `.json`, a `.jsonl`, or a directory tree
//! of OSV JSON files (e.g. an unpacked `osv.dev` / GHSA export) — parses each
//! advisory, and replaces the `vuln_advisories` table. This is the documented
//! out-of-band refresh; pgmcp never fetches advisories over the network at
//! runtime (local-only posture). `cve_supply_chain` then matches the parsed
//! dependency inventory against the imported advisories by SemVer range.

use std::path::Path;

use crate::code_analysis::vuln_match::{Advisory, parse_osv};
use crate::config::Config;
use crate::db;
use crate::db::queries::VulnAdvisoryRow;

pub async fn run(config_override: Option<&Path>, path: std::path::PathBuf) -> anyhow::Result<()> {
    let config = Config::load(config_override)?;
    crate::logging::init_cli_with_config(Some(&config));
    let pool = db::pool::create_pool(&config.database).await?;
    db::migrations::run_migrations(&pool, &config.vector).await?;

    if !path.exists() {
        anyhow::bail!("advisory dump path does not exist: {}", path.display());
    }

    let mut docs: Vec<serde_json::Value> = Vec::new();
    collect_docs(&path, &mut docs)?;
    if docs.is_empty() {
        anyhow::bail!(
            "no JSON advisory documents found under {} (expected .json / .jsonl)",
            path.display()
        );
    }

    // Parse → one row per (advisory, package, range).
    let mut rows: Vec<VulnAdvisoryRow> = Vec::new();
    let mut advisory_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for doc in &docs {
        for adv in extract_advisories(doc) {
            advisory_ids.insert(adv.id.clone());
            for r in &adv.ranges {
                rows.push(VulnAdvisoryRow {
                    advisory_id: adv.id.clone(),
                    ecosystem: adv.ecosystem.clone(),
                    package: adv.package.clone(),
                    introduced: r.introduced.clone(),
                    fixed: r.fixed.clone(),
                    last_affected: r.last_affected.clone(),
                    severity: adv.severity.clone(),
                    summary: adv.summary.clone(),
                });
            }
        }
    }

    let deleted = db::queries::clear_vuln_advisories(&pool).await?;
    // Insert in batches to keep the UNNEST arrays reasonable.
    let mut inserted = 0u64;
    for chunk in rows.chunks(2000) {
        inserted += db::queries::bulk_insert_vuln_advisories(&pool, chunk).await?;
    }

    println!(
        "Imported {inserted} advisory ranges ({} advisories) from {} document(s); cleared {deleted} prior rows.",
        advisory_ids.len(),
        docs.len()
    );
    Ok(())
}

/// Expand a single JSON document into advisories, handling the common dump
/// shapes: a bare OSV object, an array of OSV objects, or an `{"vulns": [...]}`
/// (osv.dev query response) wrapper.
fn extract_advisories(doc: &serde_json::Value) -> Vec<Advisory> {
    if let Some(arr) = doc.as_array() {
        return arr.iter().flat_map(parse_osv).collect();
    }
    if let Some(vulns) = doc.get("vulns").and_then(|v| v.as_array()) {
        return vulns.iter().flat_map(parse_osv).collect();
    }
    parse_osv(doc)
}

/// Collect JSON documents from `path`: a `.jsonl` (one doc per line), a single
/// `.json`, or every `.json`/`.jsonl` under a directory tree.
fn collect_docs(path: &Path, out: &mut Vec<serde_json::Value>) -> anyhow::Result<()> {
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let p = entry?.path();
            if p.is_dir() {
                collect_docs(&p, out)?;
            } else {
                collect_file(&p, out);
            }
        }
    } else {
        collect_file(path, out);
    }
    Ok(())
}

fn collect_file(path: &Path, out: &mut Vec<serde_json::Value>) {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext == "jsonl" {
        if let Ok(content) = std::fs::read_to_string(path) {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    out.push(v);
                }
            }
        }
    } else if ext == "json"
        && let Ok(content) = std::fs::read_to_string(path)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&content)
    {
        out.push(v);
    }
}
