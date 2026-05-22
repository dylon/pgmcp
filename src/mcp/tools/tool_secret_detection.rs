//! `tool_secret_detection` — Hardcoded-secret detection via entropy + regex
//! (SOTA Phase 6.2, Meli et al. NDSS 2019).

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::SecretDetectionParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

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
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let min_entropy = params.min_entropy.unwrap_or(4.0);
    let limit = params.limit.unwrap_or(100);

    // Known prefix patterns + generic high-entropy quoted strings.
    let prefix_re = Regex::new(
        r#"(?m)["']((?:AKIA[0-9A-Z]{16}|ghp_[A-Za-z0-9]{36}|gho_[A-Za-z0-9]{36}|sk-[A-Za-z0-9-_]{32,}|xox[bp]-[A-Za-z0-9-]+|-----BEGIN [A-Z]+ PRIVATE KEY-----))"#,
    )
    .expect("secret prefix regex");
    let highent_re = Regex::new(r#"["']([A-Za-z0-9/_+=-]{20,})["']"#).expect("high entropy regex");

    let rows: Vec<(String, Option<String>)> = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT relative_path, content FROM indexed_files WHERE project_id = $1 AND content IS NOT NULL",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Scan failed: {}", e), None))?;

    let mut findings: Vec<serde_json::Value> = Vec::new();
    'outer: for (path, content) in rows {
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
                if findings.len() >= limit.max(0) as usize {
                    break 'outer;
                }
            }
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
                    if findings.len() >= limit.max(0) as usize {
                        break 'outer;
                    }
                }
            }
        }
    }
    json_result(&json!({
        "project": params.project,
        "min_entropy": min_entropy,
        "findings": findings,
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
