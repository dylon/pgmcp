//! `tool_semver_break_audit` — Detect REMOVED public symbols across git
//! history (SOTA Phase 7.2). Compares the current public-API surface to the
//! one at `base_ref` commits ago.

#![allow(unused_imports)]

use libdictenstein::dynamic_dawg_char::DynamicDawgChar;
use liblevenshtein::transducer::Transducer;
use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::SemverBreakAuditParams;
use crate::mcp::tools::sema_helpers::signatures::{
    fetch_signature_descriptor, signature_shape_hash,
};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

const DEFAULT_SEMVER_WINDOW_COMMITS: u32 = 50;
const MAX_SEMVER_WINDOW_COMMITS: u32 = 1_000;
const DEFAULT_SEMVER_LIMIT: i32 = 50;
const MAX_SEMVER_LIMIT: i32 = 250;
const MAX_PUBLIC_SYMBOLS: i64 = 50_000;
const SEMVER_RENAME_MAX_DISTANCE: usize = 2;

pub async fn tool_semver_break_audit(
    ctx: &SystemContext,
    params: SemverBreakAuditParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "semver_break_audit", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project = params.project.trim();
    let window = params
        .window_commits
        .unwrap_or(DEFAULT_SEMVER_WINDOW_COMMITS)
        .clamp(1, MAX_SEMVER_WINDOW_COMMITS);
    let limit = params
        .limit
        .unwrap_or(DEFAULT_SEMVER_LIMIT)
        .clamp(1, MAX_SEMVER_LIMIT) as usize;

    let project_id = project_id_or_err(ctx, project).await?;
    let pool = pool_or_err(ctx)?;

    let public_symbol_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM file_symbols fs
         JOIN indexed_files f ON fs.file_id = f.id
         WHERE f.project_id = $1 AND fs.visibility = 'public'",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("API count failed: {e}"), None))?;
    if public_symbol_count > MAX_PUBLIC_SYMBOLS {
        return Err(McpError::invalid_params(
            format!(
                "project has {public_symbol_count} public symbols; max supported is {MAX_PUBLIC_SYMBOLS}"
            ),
            None,
        ));
    }

    // Current public API snapshot. Carry symbol_id so we can fetch
    // the shadow-ASR signature for each present-day public symbol.
    let now: Vec<(i64, String, String, String)> =
        sqlx::query_as::<_, (i64, String, String, String)>(
            "SELECT fs.id, f.relative_path, fs.name, fs.kind
             FROM file_symbols fs
             JOIN indexed_files f ON fs.file_id = f.id
             WHERE f.project_id = $1 AND fs.visibility = 'public'
             ORDER BY f.relative_path, fs.name, fs.kind, fs.id",
        )
        .bind(project_id)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("API query failed: {}", e), None))?;
    let now_set: BTreeSet<(String, String, String)> = now
        .iter()
        .map(|(_, path, name, kind)| (path.clone(), name.clone(), kind.clone()))
        .collect();

    // Build a "previous public API" candidate set by scanning the commit-chunk
    // text from the last N commits for public-marker patterns (Rust `pub fn` /
    // Python top-level `def` / JS `export`).
    let candidate_rows: Vec<(String,)> = sqlx::query_as::<_, (String,)>(
        "SELECT gcc.content
         FROM git_commits gc
         JOIN git_commit_chunks gcc ON gcc.commit_id = gc.id
         WHERE gc.project_id = $1
         ORDER BY gc.author_date DESC
         LIMIT $2",
    )
    .bind(project_id)
    .bind(i64::from(window))
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let pub_re = Regex::new(r"(?m)\bpub(?:\(crate\))?\s+(fn|struct|enum|trait|const|static|type)\s+([A-Za-z_][A-Za-z0-9_]*)|\bexport\s+(function|class|const|let|var|interface|enum|type)\s+([A-Za-z_][A-Za-z0-9_]*)|^def\s+([A-Za-z_][A-Za-z0-9_]*)").expect("pub regex");
    let mut historical: BTreeSet<(String, String)> = BTreeSet::new();
    for (text,) in &candidate_rows {
        for cap in pub_re.captures_iter(text) {
            let (kind, name) = if let (Some(k), Some(n)) = (cap.get(1), cap.get(2)) {
                (k.as_str().to_string(), n.as_str().to_string())
            } else if let (Some(k), Some(n)) = (cap.get(3), cap.get(4)) {
                (k.as_str().to_string(), n.as_str().to_string())
            } else if let Some(n) = cap.get(5) {
                ("function".to_string(), n.as_str().to_string())
            } else {
                continue;
            };
            historical.insert((kind, name));
        }
    }
    // Removed = in historical but not in now.
    //
    // For each removed name, find the nearest present-day name by
    // Damerau-Levenshtein distance ≤ 2 (transposition treated as one edit,
    // so `teh`/`the` is distance 1). We build a `DynamicDawgChar` once
    // over `now_names` and query via liblevenshtein's `Transducer`. The
    // automaton-based query is O(automaton-state-traversal) per probe,
    // vs the previous brute-force O(|now_names| × L²) per removed symbol.
    let now_names: BTreeSet<String> = now_set.iter().map(|(_, n, _)| n.clone()).collect();
    let now_terms: Vec<&str> = now_names.iter().map(|s| s.as_str()).collect();
    let now_dict: DynamicDawgChar<()> = DynamicDawgChar::from_terms(now_terms);
    let now_transducer = Transducer::with_transposition(now_dict);

    let mut removed: Vec<(String, String, Option<String>)> = Vec::new();
    for (kind, name) in &historical {
        if !now_names.contains(name) {
            // Phase 10 re-rank: collect every Damerau-Levenshtein
            // candidate within distance 2, then pick the one with the
            // lowest articulatory_edit_distance from the original name.
            // `articulatory_edit_distance` (liblevenshtein phonetic
            // framework) charges per-character substitution at the
            // IPA articulatory-feature distance — `recieve`/`receive`
            // (voicing-preserving) ranks above `reciver`/`receive`
            // (consonant-cluster change). Plan reference:
            // ~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md
            // Phase 10.
            let likely_rename = now_transducer
                .query_with_distance(name, SEMVER_RENAME_MAX_DISTANCE)
                .min_by(|a, b| {
                    let aad = crate::fuzzy::phonetic::articulatory_distance_score(name, &a.term);
                    let bad = crate::fuzzy::phonetic::articulatory_distance_score(name, &b.term);
                    aad.partial_cmp(&bad)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.distance.cmp(&b.distance))
                        .then_with(|| a.term.cmp(&b.term))
                })
                .map(|c| c.term);
            removed.push((kind.clone(), name.clone(), likely_rename));
        }
    }
    removed.truncate(limit);
    let rows_json: Vec<_> = removed
        .into_iter()
        .map(|(k, n, r)| {
            json!({
                "kind": k,
                "name": n,
                "likely_rename_to": r,
                "severity": if r.is_some() { "major (renamed)" } else { "major (removed)" },
            })
        })
        .collect();
    // Shadow-ASR channel: for each present-day public function, surface
    // its structured SignatureDescriptor (parameters, return_type,
    // effects, signature_shape_hash). Consumers compare against their
    // own stored historical descriptors to compute exact signature_diff
    // via `sema_helpers::signatures::signature_diff`.
    let mut current_signatures: Vec<serde_json::Value> = Vec::new();
    for (id, _path, _name, kind) in &now {
        if kind != "function" {
            continue;
        }
        if let Ok(Some(desc)) = fetch_signature_descriptor(pool, *id).await {
            let hash = signature_shape_hash(&desc);
            current_signatures.push(json!({
                "symbol_id": desc.symbol_id,
                "name": desc.name,
                "scope_path": desc.scope_path,
                "signature_shape_hash": hash,
                "parameters": desc.parameters,
                "return_type": {
                    "type_raw": desc.return_type_raw,
                    "type_tags": desc.return_type_tags,
                },
                "effects": desc.effects,
            }));
        }
    }

    json_result(&json!({
        "project": project,
        "window_commits": window,
        "limit": limit,
        "removed_or_renamed": rows_json,
        "current_public_signatures": current_signatures,
        "guidance": "Removed/renamed public symbols are major-version breakages under semver. Rename candidates within Levenshtein <= 2 are flagged for clarification. The `current_public_signatures` channel carries structured signature descriptors (with `signature_shape_hash`) for every present-day public function — consumers can persist these per-release and diff via the canonical sema_helpers::signatures::signature_diff for precise breaking-change classification."
    }))
}
