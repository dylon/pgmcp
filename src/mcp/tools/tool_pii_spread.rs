//! `tool_pii_spread` — PII-shaped literals and PII-named identifiers cross-referenced
//! with logging/network sinks (SOTA Phase 9.3).
//!
//! No rust-bert dependency is required: PII identifier names are matched via a
//! curated allowlist (`PII_TOKENS`) so the tool works in offline environments.

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::PiiSpreadParams;
use crate::mcp::tools::sema_helpers::effects::symbols_with_any_effect;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::parsing::type_tags::vocabulary::{EFFECT_DATABASE, EFFECT_IO, EFFECT_NETWORK};

const PII_TOKENS: &[&str] = &[
    "ssn",
    "social_security",
    "social-security",
    "email",
    "e_mail",
    "emailaddress",
    "phone",
    "phone_number",
    "telephone",
    "dob",
    "date_of_birth",
    "birthdate",
    "birthday",
    "passport",
    "drivers_license",
    "drivers-license",
    "credit_card",
    "credit-card",
    "cc_number",
    "card_number",
    "address",
    "street_address",
    "first_name",
    "firstname",
    "given_name",
    "last_name",
    "lastname",
    "family_name",
    "surname",
    "tax_id",
    "tin",
    "ein",
    "ip_address",
    "ipaddress",
];

pub async fn tool_pii_spread(
    ctx: &SystemContext,
    params: PiiSpreadParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "pii_spread", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50);

    let pii_literal_re = Regex::new(
        r"(?m)(\b\d{3}-\d{2}-\d{4}\b|\b\d{3}\s\d{2}\s\d{4}\b|\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b|\b\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}\b)"
    ).expect("pii lit regex");
    let log_re = Regex::new(
        r"(?m)\b(tracing::(?:debug|info|warn|error)!|log::(?:debug|info|warn|error)!|logger\.(?:debug|info|warn|error)|console\.log|println!|fmt\.Println|fmt\.Printf|print\(|System\.out\.print)"
    ).expect("log regex");
    let net_re = Regex::new(
        r"(?m)\b(reqwest::Body|HttpResponse::Json|res\.json|res\.send|fetch\(|axios\.|requests\.post|requests\.put)"
    ).expect("net regex");

    let pii_ident_re = {
        let alt = PII_TOKENS.join("|");
        Regex::new(&format!(r"(?im)\b({})\b", alt)).expect("pii ident regex")
    };

    let rows: Vec<(String, String, Option<String>)> =
        sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT relative_path, language, content
             FROM indexed_files
             WHERE project_id = $1 AND content IS NOT NULL",
        )
        .bind(project_id)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Scan failed: {}", e), None))?;

    let scope = params.scope.as_deref().unwrap_or("all");

    let mut findings: Vec<serde_json::Value> = Vec::new();
    for (path, _lang, content) in rows {
        let Some(c) = content else { continue };
        // PII literals
        for m in pii_literal_re.find_iter(&c) {
            let line = c[..m.start()].bytes().filter(|b| *b == b'\n').count() + 1;
            findings.push(json!({
                "file": path,
                "line": line,
                "kind": "pii_literal",
                "preview": &m.as_str()[..m.as_str().len().min(40)],
            }));
            if findings.len() >= limit.max(0) as usize {
                break;
            }
        }
        if findings.len() >= limit.max(0) as usize {
            break;
        }
        // PII identifiers near sinks
        let has_pii_ident = pii_ident_re.is_match(&c);
        if !has_pii_ident {
            continue;
        }
        let scan_log = scope == "all" || scope == "logs";
        let scan_net = scope == "all" || scope == "network";
        if scan_log {
            for m in log_re.find_iter(&c) {
                let line = c[..m.start()].bytes().filter(|b| *b == b'\n').count() + 1;
                let line_start = c[..m.start()].rfind('\n').map(|i| i + 1).unwrap_or(0);
                let line_end = c[m.start()..]
                    .find('\n')
                    .map(|i| m.start() + i)
                    .unwrap_or_else(|| c.len());
                let snip = &c[line_start..line_end];
                if pii_ident_re.is_match(snip) {
                    findings.push(json!({
                        "file": path,
                        "line": line,
                        "kind": "pii_logged",
                        "snippet": snip.trim().chars().take(160).collect::<String>(),
                    }));
                    if findings.len() >= limit.max(0) as usize {
                        break;
                    }
                }
            }
        }
        if findings.len() >= limit.max(0) as usize {
            break;
        }
        if scan_net {
            for m in net_re.find_iter(&c) {
                let line = c[..m.start()].bytes().filter(|b| *b == b'\n').count() + 1;
                let line_start = c[..m.start()].rfind('\n').map(|i| i + 1).unwrap_or(0);
                let line_end = c[m.start()..]
                    .find('\n')
                    .map(|i| m.start() + i)
                    .unwrap_or_else(|| c.len());
                let snip = &c[line_start..line_end];
                if pii_ident_re.is_match(snip) {
                    findings.push(json!({
                        "file": path,
                        "line": line,
                        "kind": "pii_egressed",
                        "snippet": snip.trim().chars().take(160).collect::<String>(),
                    }));
                    if findings.len() >= limit.max(0) as usize {
                        break;
                    }
                }
            }
        }
        if findings.len() >= limit.max(0) as usize {
            break;
        }
    }
    // Shadow-ASR channel: symbols touching network/database/IO effects —
    // candidate sinks where PII might leak after flowing through.
    let sink_effect_symbols = symbols_with_any_effect(
        pool,
        project_id,
        &[
            EFFECT_NETWORK.to_string(),
            EFFECT_DATABASE.to_string(),
            EFFECT_IO.to_string(),
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
    json_result(&json!({
        "project": params.project,
        "scope": scope,
        "findings": findings,
        "sink_effect_symbols": sink_effect_symbols,
        "guidance": "Surfaces (1) PII-shaped literals (SSN, email, IP) and (2) PII-named identifiers co-located with logging/network sinks. Curated PII_TOKENS list — no external NER required."
    }))
}
