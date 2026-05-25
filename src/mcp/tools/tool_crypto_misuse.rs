//! `tool_crypto_misuse` — crypto-misuse detection (graph-roadmap Phase 2.2;
//! CryptoLint CCS 2013, CryptoGuard ICSE 2019).
//!
//! `ast_findings` are AST-matched (tree-sitter) — precise, immune to matches
//! inside comments/strings (Python today). `heuristic_findings` keep the
//! labeled-regex rules for languages without an AST rule set.

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use sqlx::PgPool;
use std::sync::atomic::Ordering;

use crate::code_analysis::ast_rules::{self, AstRuleHit};
use crate::context::SystemContext;
use crate::mcp::server::CryptoMisuseParams;
use crate::mcp::tools::sema_helpers::effects::symbols_with_any_effect;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::mcp::tools::sota_regex_scan::scan_files_for_pattern;
use crate::parsing::type_tags::vocabulary::{EFFECT_CRYPTO, EFFECT_CRYPTO_WEAK};

/// Run the AST rule engine over every rule-capable file in the project.
/// Shared by `crypto_misuse` and `unsafe_deserialization` (each filters by
/// `AstRuleHit::category`).
pub(crate) async fn scan_project_ast_rules(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<(String, String, AstRuleHit)>, sqlx::Error> {
    let rows: Vec<(String, String, Option<String>)> =
        sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT relative_path, language, content
             FROM indexed_files
             WHERE project_id = $1 AND content IS NOT NULL AND language = ANY($2)",
        )
        .bind(project_id)
        .bind(ast_rules::AST_RULE_LANGUAGES)
        .fetch_all(pool)
        .await?;
    let mut out = Vec::new();
    for (path, lang, content) in rows {
        let Some(c) = content else { continue };
        for hit in ast_rules::scan(&lang, &c) {
            out.push((path.clone(), lang.clone(), hit));
        }
    }
    Ok(out)
}

pub async fn tool_crypto_misuse(
    ctx: &SystemContext,
    params: CryptoMisuseParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "crypto_misuse", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50).max(0) as usize;

    // Precise AST findings (crypto category).
    let mut ast_findings: Vec<serde_json::Value> = scan_project_ast_rules(pool, project_id)
        .await
        .map_err(|e| McpError::internal_error(format!("AST scan failed: {}", e), None))?
        .into_iter()
        .filter(|(_, _, h)| h.category == "crypto")
        .map(|(path, lang, h)| {
            json!({
                "rule": h.rule_id, "file": path, "language": lang,
                "line": h.line, "message": h.message, "snippet": h.snippet,
            })
        })
        .collect();
    ast_findings.truncate(limit);

    // Regex heuristic for languages WITHOUT an AST rule set.
    let rules: &[(&str, &str)] = &[
        ("ecb_mode", r"(?i)(AES|DES)[^A-Za-z]+ECB|Mode::ECB"),
        ("md5_in_security", r"(?m)\b(md5|MD5)\s*[\(\!\[]"),
        ("sha1_in_security", r"(?m)\b(sha1|SHA1)\s*[\(\!\[]"),
        (
            "weak_random_for_token",
            r"(?m)\b(Math\.random|rand::thread_rng|rand\.Rand|random\.random)\b",
        ),
        ("static_iv", r#"(?m)IV\s*=\s*["'][0-9A-Za-z]{8,}["']"#),
        (
            "hardcoded_crypto_key",
            r#"(?mi)\b(secret_key|api_secret|hmac_key|signing_key)\s*=\s*["'][^"']{8,}["']"#,
        ),
        (
            "base64_decoded_secret",
            r"(?m)\b(base64_decode|atob|Base64::decode)\(",
        ),
    ];
    let mut heuristic_findings: Vec<serde_json::Value> = Vec::new();
    for (label, p) in rules {
        if heuristic_findings.len() >= limit {
            break;
        }
        let re = Regex::new(p).expect("rule regex");
        let hits = scan_files_for_pattern(pool, project_id, &re, None, limit)
            .await
            .map_err(|e| McpError::internal_error(format!("Scan failed: {}", e), None))?;
        for h in hits {
            if ast_rules::has_rules(&h.language) {
                continue; // AST owns these languages
            }
            heuristic_findings.push(json!({
                "rule": label, "file": h.relative_path, "language": h.language,
                "line": h.line, "snippet": h.snippet,
            }));
            if heuristic_findings.len() >= limit {
                break;
            }
        }
    }

    let effect_symbols = symbols_with_any_effect(
        pool,
        project_id,
        &[EFFECT_CRYPTO.to_string(), EFFECT_CRYPTO_WEAK.to_string()],
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

    json_result(&json!({
        "project": params.project,
        "ast_findings": ast_findings,
        "heuristic_findings": heuristic_findings,
        "effect_symbols": effect_symbols,
        "guidance": "`ast_findings` are tree-sitter AST matches (precise — never match inside comments/strings, \
            and inspect argument shape, e.g. `yaml.load` without a safe Loader). `heuristic_findings` are the \
            labeled-regex rules for languages without an AST rule set yet. Patterns: ECB mode, MD5/SHA-1 for \
            security, non-secure RNG for tokens, static IVs, hardcoded keys (CryptoLint CCS 2013, CryptoGuard \
            ICSE 2019)."
    }))
}
