//! `tool_secret_detection` — Hardcoded-secret detection via entropy + regex
//! (SOTA Phase 6.2, Meli et al. NDSS 2019).

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use futures::TryStreamExt;

use crate::context::SystemContext;
use crate::mcp::server::SecretDetectionParams;
use crate::mcp::tools::sema_helpers::effects::symbols_with_any_effect;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::parsing::type_tags::vocabulary::{EFFECT_CRYPTO, EFFECT_CRYPTO_WEAK};

const DEFAULT_SECRET_MIN_ENTROPY: f64 = 4.0;
const MAX_SECRET_MIN_ENTROPY: f64 = 8.0;
const DEFAULT_SECRET_FINDING_LIMIT: i32 = 100;
const MAX_SECRET_FINDING_LIMIT: i32 = 500;

/// Shannon entropy of a byte string (uses base-2 log; max for 64 distinct
/// chars ≈ 6.0). Hardcoded keys typically score above 4.0.
fn shannon_entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut freq = [0u32; 256];
    for &b in s.as_bytes() {
        freq[b as usize] += 1;
    }
    let n = s.len() as f64;
    let mut h = 0.0;
    for &c in freq.iter() {
        if c == 0 {
            continue;
        }
        let p = c as f64 / n;
        h -= p * p.log2();
    }
    h
}

pub async fn tool_secret_detection(
    ctx: &SystemContext,
    params: SecretDetectionParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "secret_detection", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project = params.project.trim();
    let min_entropy = params.min_entropy.unwrap_or(DEFAULT_SECRET_MIN_ENTROPY);
    if !min_entropy.is_finite() {
        return Err(McpError::invalid_params("min_entropy must be finite", None));
    }
    let min_entropy = min_entropy.clamp(0.0, MAX_SECRET_MIN_ENTROPY);
    let limit = params
        .limit
        .unwrap_or(DEFAULT_SECRET_FINDING_LIMIT)
        .clamp(1, MAX_SECRET_FINDING_LIMIT) as usize;

    let project_id = project_id_or_err(ctx, project).await?;
    let pool = pool_or_err(ctx)?;

    // Known prefix patterns + generic high-entropy quoted strings.
    let prefix_re = Regex::new(
        r#"(?m)["']((?:AKIA[0-9A-Z]{16}|ghp_[A-Za-z0-9]{36}|gho_[A-Za-z0-9]{36}|sk-[A-Za-z0-9-_]{32,}|xox[bp]-[A-Za-z0-9-]+|-----BEGIN [A-Z]+ PRIVATE KEY-----))"#,
    )
    .expect("secret prefix regex");
    let highent_re = Regex::new(r#"["']([A-Za-z0-9/_+=-]{20,})["']"#).expect("high entropy regex");

    let mut rows = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT relative_path, content FROM indexed_files
         WHERE project_id = $1 AND content IS NOT NULL
         ORDER BY id",
    )
    .bind(project_id)
    .fetch(pool);

    let mut findings: Vec<serde_json::Value> = Vec::new();
    while findings.len() < limit {
        let Some((path, content)) = rows
            .try_next()
            .await
            .map_err(|e| McpError::internal_error(format!("Scan failed: {e}"), None))?
        else {
            break;
        };
        let Some(c) = content else { continue };
        for cap in prefix_re.captures_iter(&c) {
            if let Some(secret) = cap.get(1) {
                let line = c[..secret.start()].bytes().filter(|b| *b == b'\n').count() + 1;
                findings.push(json!({
                    "file": path,
                    "line": line,
                    "kind": "known-prefix",
                    "preview": &secret.as_str()[..secret.as_str().len().min(8)],
                }));
                if findings.len() >= limit {
                    break;
                }
            }
        }
        if findings.len() >= limit {
            break;
        }
        for cap in highent_re.captures_iter(&c) {
            if let Some(s) = cap.get(1) {
                let txt = s.as_str();
                let h = shannon_entropy(txt);
                if h >= min_entropy {
                    let line = c[..s.start()].bytes().filter(|b| *b == b'\n').count() + 1;
                    findings.push(json!({
                        "file": path,
                        "line": line,
                        "kind": "high-entropy",
                        "entropy": h,
                        "len": txt.len(),
                    }));
                    if findings.len() >= limit {
                        break;
                    }
                }
            }
        }
    }
    drop(rows);

    // Shadow-ASR channel: symbols flagged with crypto effects — these are
    // priority targets for secret-detection review (the symbol's body or
    // arguments likely touch crypto material).
    let crypto_symbols = symbols_with_any_effect(
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
        "project": project,
        "min_entropy": min_entropy,
        "limit": limit,
        "findings": findings,
        "crypto_symbols": crypto_symbols,
        "guidance": "Combines regex prefix-matching (AWS keys, GitHub PATs, OpenAI keys, Slack tokens, PEM headers) with Shannon entropy ≥ threshold on string literals. Review preview bytes carefully — false positives are possible on base64 test data."
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn entropy_uniform_max() {
        // 64 distinct chars in equal proportion → H = log2(64) = 6.0
        let s: String =
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".to_string();
        let h = shannon_entropy(&s);
        assert!(h > 5.0);
    }
    #[test]
    fn entropy_constant_is_zero() {
        assert_eq!(shannon_entropy("AAAAAA"), 0.0);
    }
    #[test]
    fn entropy_empty_is_zero() {
        assert_eq!(shannon_entropy(""), 0.0);
    }
}
