//! `tool_crypto_misuse` — Crypto-misuse rules (SOTA Phase 6.3, CryptoLint CCS 2013).

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::CryptoMisuseParams;
use crate::mcp::tools::sema_helpers::effects::symbols_with_any_effect;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::mcp::tools::sota_regex_scan::scan_files_for_pattern;
use crate::parsing::type_tags::vocabulary::{EFFECT_CRYPTO, EFFECT_CRYPTO_WEAK};

pub async fn tool_crypto_misuse(
    ctx: &SystemContext,
    params: CryptoMisuseParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "crypto_misuse", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50);

    // Build a labeled rule set.
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
    let mut findings: Vec<serde_json::Value> = Vec::new();
    for (label, p) in rules {
        let re = Regex::new(p).expect("rule regex");
        let hits = scan_files_for_pattern(pool, project_id, &re, None, limit.max(0) as usize)
            .await
            .map_err(|e| McpError::internal_error(format!("Scan failed: {}", e), None))?;
        for h in hits {
            findings.push(json!({
                "rule": label,
                "file": h.relative_path,
                "language": h.language,
                "line": h.line,
                "snippet": h.snippet,
            }));
            if findings.len() >= limit.max(0) as usize {
                break;
            }
        }
        if findings.len() >= limit.max(0) as usize {
            break;
        }
    }
    // Shadow-ASR channel: symbols carrying crypto-related effects.
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
        "findings": findings,
        "effect_symbols": effect_symbols,
        "guidance": "CryptoLint CCS 2013 + CryptoGuard ICSE 2019 surface common crypto-misuse patterns: ECB mode, MD5/SHA-1 in auth, non-secure RNG for tokens, static IVs, hardcoded keys."
    }))
}
