//! Security collectors. Most are content-regex scans over `indexed_files`
//! (mirroring the on-demand security tools); `cve_supply_chain` matches imported
//! packages against the `vuln_advisories` table.
//!
//! `semver_break_audit` is intentionally not collected here: it requires
//! cross-release public-API diffing, and pgmcp tracks no release/version
//! baseline to diff against — so there is genuinely nothing to compute rather
//! than a faked signal. (The aggregator simply omits it.)

use std::collections::HashMap;

use regex::Regex;
use rmcp::ErrorData as McpError;
use serde_json::json;

use super::truncate_preview;
use crate::context::SystemContext;
use crate::mcp::tools::sota_helpers::pool_or_err;
use crate::quality::findings::{Finding, FindingCategory, Severity};

const SEC: FindingCategory = FindingCategory::Security;

/// Fetch (path, content) for every text file in the project, once.
async fn project_contents(
    ctx: &SystemContext,
    project_id: i32,
) -> Result<Vec<(String, String)>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        relative_path: String,
        content: Option<String>,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT relative_path, content FROM indexed_files
         WHERE project_id = $1 AND content IS NOT NULL",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("content fetch failed: {e}"), None))?;
    Ok(rows
        .into_iter()
        .filter_map(|r| r.content.map(|c| (r.relative_path, c)))
        .collect())
}

/// Shannon entropy (bits/char) of a string — used to flag high-entropy literals.
fn shannon_entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts: HashMap<char, usize> = HashMap::new();
    for c in s.chars() {
        *counts.entry(c).or_insert(0) += 1;
    }
    let len = s.chars().count() as f64;
    -counts
        .values()
        .map(|&n| {
            let p = n as f64 / len;
            p * p.log2()
        })
        .sum::<f64>()
}

/// Hardcoded secrets — known token prefixes (Critical) and high-entropy quoted
/// literals (High/Medium).
pub async fn collect_secret_detection(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let contents = project_contents(ctx, project_id).await?;
    let known = Regex::new(
        r"(AKIA[0-9A-Z]{16}|gh[pous]_[A-Za-z0-9]{20,}|sk-[A-Za-z0-9]{20,}|xox[bpas]-[A-Za-z0-9-]{10,}|-----BEGIN [A-Z ]*PRIVATE KEY-----)",
    )
    .expect("valid regex");
    let quoted = Regex::new(r#"["']([A-Za-z0-9+/=_\-]{24,})["']"#).expect("valid regex");

    let mut out = Vec::new();
    for (path, content) in &contents {
        for (i, line) in content.lines().enumerate() {
            if let Some(m) = known.find(line) {
                out.push(
                    Finding::new(
                        "secret_detection",
                        SEC,
                        project_name,
                        Severity::Critical,
                        format!(
                            "Hardcoded secret (known prefix): {}",
                            truncate_preview(m.as_str(), 16)
                        ),
                    )
                    .at(path, (i + 1) as u32)
                    .with_kind("hardcoded_secret_known_prefix")
                    .with_raw(json!({ "file": path, "line": i + 1, "kind": "known-prefix" })),
                );
                continue;
            }
            if let Some(c) = quoted.captures(line) {
                let lit = &c[1];
                let ent = shannon_entropy(lit);
                if ent >= 4.5 {
                    let severity = if ent >= 5.0 {
                        Severity::High
                    } else {
                        Severity::Medium
                    };
                    out.push(
                        Finding::new(
                            "secret_detection",
                            SEC,
                            project_name,
                            severity,
                            format!("High-entropy literal (entropy {ent:.1}) — possible secret"),
                        )
                        .with_score(ent)
                        .at(path, (i + 1) as u32)
                        .with_kind("hardcoded_secret_high_entropy")
                        .with_raw(json!({ "file": path, "line": i + 1, "entropy": ent })),
                    );
                }
            }
        }
    }
    Ok(out)
}

/// Run a set of `(regex, kind, severity, blurb)` rules line-by-line over project
/// content, producing one finding per match. The shared engine behind several
/// regex-based security collectors.
async fn regex_scan(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
    tool: &'static str,
    rules: &[(Regex, &'static str, Severity, &'static str)],
) -> Result<Vec<Finding>, McpError> {
    let contents = project_contents(ctx, project_id).await?;
    let mut out = Vec::new();
    for (path, content) in &contents {
        for (i, line) in content.lines().enumerate() {
            for (re, kind, severity, blurb) in rules {
                if re.is_match(line) {
                    out.push(
                        Finding::new(
                            tool,
                            SEC,
                            project_name,
                            *severity,
                            format!("{blurb}: {}", truncate_preview(line, 100)),
                        )
                        .at(path, (i + 1) as u32)
                        .with_kind(*kind)
                        .with_raw(json!({ "file": path, "line": i + 1, "kind": kind })),
                    );
                    break; // one finding per line is enough
                }
            }
        }
    }
    Ok(out)
}

/// Injection review-candidates — string-built queries / shell commands.
pub async fn collect_injection_candidates(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let rules = vec![
        (
            Regex::new(r#"(?i)(format!|"\s*\+|f")[^;]*\b(SELECT|INSERT|UPDATE|DELETE)\b"#)
                .expect("re"),
            "sql_string_build",
            Severity::Medium,
            "Possible SQL built from a string",
        ),
        (
            Regex::new(r#"(?i)(system|exec|popen|Command::new)\s*\([^)]*(\+|format!|\$\{|f")"#)
                .expect("re"),
            "shell_interpolation",
            Severity::Medium,
            "Shell command with interpolation",
        ),
    ];
    regex_scan(
        ctx,
        project_id,
        project_name,
        "injection_candidates",
        &rules,
    )
    .await
}

/// Weak/broken cryptography.
pub async fn collect_crypto_misuse(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let rules = vec![
        (
            Regex::new(r"(?i)\b(MD5|SHA1|DES|RC4)\b").expect("re"),
            "weak_algorithm",
            Severity::High,
            "Weak cryptographic algorithm",
        ),
        (
            Regex::new(r"(?i)(ECB|Math\.random|new Random\(\))").expect("re"),
            "insecure_mode_or_rng",
            Severity::High,
            "Insecure cipher mode or non-crypto RNG",
        ),
    ];
    regex_scan(ctx, project_id, project_name, "crypto_misuse", &rules).await
}

/// Unsafe deserialization sinks.
pub async fn collect_unsafe_deserialization(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let rules = vec![(
        // NB: the `regex` crate has no look-around, so we match `yaml.load(`
        // unconditionally (safe `Loader=`/`safe_load` callers are a tolerable
        // false-positive for a review-candidate finding).
        Regex::new(
            r"(?i)(pickle\.loads|yaml\.load\s*\(|ObjectInputStream|unserialize\s*\(|Marshal\.load)",
        )
        .expect("re"),
        "unsafe_deserialization",
        Severity::High,
        "Unsafe deserialization sink",
    )];
    regex_scan(
        ctx,
        project_id,
        project_name,
        "unsafe_deserialization",
        &rules,
    )
    .await
}

/// PII literals / PII reaching logs.
pub async fn collect_pii_spread(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let contents = project_contents(ctx, project_id).await?;
    let ssn = Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").expect("re");
    let email = Regex::new(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}").expect("re");
    let log = Regex::new(r"(?i)\b(log|logger|println!|console\.(log|error)|print\()").expect("re");
    let mut out = Vec::new();
    for (path, content) in &contents {
        for (i, line) in content.lines().enumerate() {
            let has_pii = ssn.is_match(line) || email.is_match(line);
            if !has_pii {
                continue;
            }
            let logged = log.is_match(line);
            let (kind, severity) = if logged {
                ("pii_logged", Severity::High)
            } else {
                ("pii_literal", Severity::Low)
            };
            out.push(
                Finding::new(
                    "pii_spread",
                    SEC,
                    project_name,
                    severity,
                    format!(
                        "{}: {}",
                        if logged {
                            "PII reaching a log sink"
                        } else {
                            "PII literal"
                        },
                        truncate_preview(line, 80)
                    ),
                )
                .at(path, (i + 1) as u32)
                .with_kind(kind)
                .with_raw(json!({ "file": path, "line": i + 1, "kind": kind })),
            );
        }
    }
    Ok(out)
}

/// Taint review-candidates — files that contain both a tainted source and a
/// dangerous sink (file-level co-occurrence heuristic).
pub async fn collect_taint_analysis(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let contents = project_contents(ctx, project_id).await?;
    let source = Regex::new(
        r"(?i)(request\.|req\.(body|query|params)|input\s*\(|argv|env::var|getenv|read_line)",
    )
    .expect("re");
    let sink = Regex::new(
        r"(?i)(\.execute\s*\(|system\s*\(|exec\s*\(|eval\s*\(|Command::new|\.query\s*\()",
    )
    .expect("re");
    let mut out = Vec::new();
    for (path, content) in &contents {
        if source.is_match(content) && sink.is_match(content) {
            out.push(
                Finding::new(
                    "taint_analysis",
                    SEC,
                    project_name,
                    Severity::Medium,
                    format!(
                        "{path} mixes external input with a dangerous sink — review for taint flow"
                    ),
                )
                .at_file(path)
                .with_kind("taint_cooccurrence")
                .with_raw(json!({ "file": path })),
            );
        }
    }
    Ok(out)
}

/// Route definitions — surfaced as review candidates for missing auth.
pub async fn collect_unprotected_routes(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let rules = vec![(
        Regex::new(r#"(?i)(#\[(get|post|put|delete|patch)\(|@app\.route|router\.(get|post|put|delete)|app\.(get|post|put|delete)\s*\()"#)
            .expect("re"),
        "route_definition",
        Severity::Low,
        "Route handler — verify authorization",
    )];
    regex_scan(ctx, project_id, project_name, "unprotected_routes", &rules).await
}

/// Attack-surface — files that are both externally reachable (a route/handler)
/// and reach a dangerous sink.
pub async fn collect_attack_vulnerability(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let contents = project_contents(ctx, project_id).await?;
    let entry = Regex::new(
        r#"(?i)(#\[(get|post|put|delete)\(|@app\.route|router\.(get|post)|fn main\b|handler)"#,
    )
    .expect("re");
    let sink = Regex::new(
        r"(?i)(\.execute\s*\(|system\s*\(|exec\s*\(|eval\s*\(|Command::new|unsafe\s*\{)",
    )
    .expect("re");
    let mut out = Vec::new();
    for (path, content) in &contents {
        if entry.is_match(content) && sink.is_match(content) {
            out.push(
                Finding::new(
                    "attack_vulnerability",
                    SEC,
                    project_name,
                    Severity::Medium,
                    format!("{path} is externally reachable and reaches a dangerous sink"),
                )
                .at_file(path)
                .with_kind("reachable_sink")
                .with_raw(json!({ "file": path })),
            );
        }
    }
    Ok(out)
}

/// Imported packages matching known vulnerability advisories. Empty (data-absent)
/// until OSV advisories are imported into `vuln_advisories`.
pub async fn collect_cve_supply_chain(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        package: String,
        advisory_id: String,
        severity: Option<String>,
        summary: Option<String>,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT DISTINCT va.package, va.advisory_id, va.severity, va.summary
         FROM code_graph_edges cge
         JOIN vuln_advisories va
              ON cge.target_raw ILIKE '%' || va.package || '%'
         WHERE cge.project_id = $1 AND cge.edge_type = 'import'
           AND cge.target_file_id IS NULL
         LIMIT 500",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("cve_supply_chain query failed: {e}"), None))?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let severity = match r.severity.as_deref().map(|s| s.to_ascii_uppercase()) {
                Some(ref s) if s.contains("CRITICAL") => Severity::Critical,
                Some(ref s) if s.contains("HIGH") => Severity::High,
                Some(ref s) if s.contains("MODERATE") || s.contains("MEDIUM") => Severity::Medium,
                _ => Severity::Low,
            };
            Finding::new(
                "cve_supply_chain",
                SEC,
                project_name,
                severity,
                format!(
                    "Dependency `{}` matches advisory {} — {}",
                    r.package,
                    r.advisory_id,
                    truncate_preview(r.summary.as_deref().unwrap_or(""), 80)
                ),
            )
            .with_kind(format!("advisory:{}", r.advisory_id))
            .with_raw(
                json!({ "package": r.package, "advisory": r.advisory_id, "severity": r.severity }),
            )
        })
        .collect())
}
