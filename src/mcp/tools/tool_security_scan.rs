//! `security_scan` — run installed external security scanners over the indexed
//! projects and query their findings.
//!
//! Read path (default): returns cached findings from `external_scanner_findings`
//! (severity-floored, optionally scoped to a project / scanner subset). Refresh
//! path (`refresh=true`): acquires the heavy-cron lock and runs the scanner sweep
//! (`crate::cron::security_scan`) first — a subprocess pass over the project
//! root(s) — then returns the freshly-upserted findings.
//!
//! Findings are advisory. Enabling `[tracker] auto_promote_findings` lets the
//! findings-promotion cron materialize high/critical findings into `pending`
//! `bug` work items (never self-verifying — only the user's triage + CI evidence
//! reach `verified`).

#![allow(unused_imports)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::SecurityScanParams;
use crate::mcp::tools::sota_helpers::json_result;
use crate::tracker::severity::Severity;

fn sev_rank(s: &str) -> i32 {
    match s {
        "critical" => 4,
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

pub async fn tool_security_scan(
    ctx: &SystemContext,
    params: SecurityScanParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    let pool = ctx.db().pool().cloned().ok_or_else(|| {
        McpError::internal_error("security_scan needs Postgres (no PgPool)", None)
    })?;
    let cfg = ctx.config().load().security_scan.clone();

    let project = params
        .project
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string);
    let refresh = params.refresh.unwrap_or(false);
    let limit = params.limit.unwrap_or(100).clamp(1, 2000);
    let status_filter = if params.include_resolved.unwrap_or(false) {
        None
    } else {
        Some("open")
    };
    let min_rank = match params.severity_min.as_deref() {
        Some(s) => Severity::parse(&s.to_ascii_lowercase()).map_or(0, |sv| sv.rank() as i32),
        None => 0,
    };
    let scanners = params.scanners.clone();
    // Default to security findings; lint findings (finding_class='lint', posted by
    // the crucible linter loop via POST /api/scanner/findings) are excluded unless
    // explicitly requested, so they never masquerade as vulnerabilities (ADR-014).
    let finding_class_filter: Option<&str> = params.finding_class.as_deref().or(Some("security"));

    // --- optional refresh: run the scanners now (heavy — spawns subprocesses) ---
    let mut scan_report: Option<Value> = None;
    if refresh {
        let _heavy_guard = match ctx.heavy_cron_lock().try_lock() {
            Ok(g) => g,
            Err(_) => {
                return json_result(&json!({
                    "status": "busy",
                    "retry_after_secs": 60,
                    "guidance": "Another heavy cron is running; retry the refresh shortly. The cached read path (refresh=false) is always available.",
                }));
            }
        };
        let _cron_flag = crate::cron::scheduler::HeavyCronFlag::new(Arc::clone(ctx.stats()));
        let report =
            crate::cron::security_scan::run_security_scan(&pool, &cfg, project.as_deref()).await;
        report.log_summary();
        scan_report = Some(json!({
            "projects_scanned": report.projects_scanned,
            "findings_upserted": report.findings_upserted,
            "findings_resolved": report.findings_resolved,
            "runs_ok": report.runs_ok,
            "runs_timeout": report.runs_timeout,
            "runs_error": report.runs_error,
            "runs_absent": report.runs_absent,
            "runs_skipped": report.runs_skipped,
            "scanners_available": report.scanners_available,
            "scanners_missing": report.scanners_missing,
            "scanners_skipped_offline": report.scanners_skipped_offline,
        }));
    }

    // --- resolve the project filter to concrete ids (None = all projects) ---
    let project_ids: Option<Vec<i32>> = if let Some(filter) = &project {
        let projects = queries::list_projects(&pool)
            .await
            .map_err(|e| McpError::internal_error(format!("list_projects: {e}"), None))?;
        let fl = filter.to_ascii_lowercase();
        Some(
            projects
                .into_iter()
                .filter(|p| {
                    p.name.to_ascii_lowercase().contains(&fl)
                        || p.path.to_ascii_lowercase().contains(&fl)
                })
                .map(|p| p.id)
                .collect(),
        )
    } else {
        None
    };

    // --- query findings ---
    let scanners_ref = scanners.as_deref();
    let mut rows: Vec<queries::ScannerFindingRow> = Vec::new();
    match &project_ids {
        Some(ids) if ids.is_empty() => {
            // project filter matched nothing — leave rows empty.
        }
        Some(ids) => {
            for &id in ids {
                let part = queries::query_scanner_findings(
                    &pool,
                    Some(id),
                    scanners_ref,
                    min_rank,
                    status_filter,
                    limit,
                    finding_class_filter,
                )
                .await
                .map_err(|e| McpError::internal_error(format!("query findings: {e}"), None))?;
                rows.extend(part);
            }
            rows.sort_by(|a, b| {
                sev_rank(&b.severity)
                    .cmp(&sev_rank(&a.severity))
                    .then(b.last_seen_at.cmp(&a.last_seen_at))
            });
            rows.truncate(limit as usize);
        }
        None => {
            rows = queries::query_scanner_findings(
                &pool,
                None,
                scanners_ref,
                min_rank,
                status_filter,
                limit,
                finding_class_filter,
            )
            .await
            .map_err(|e| McpError::internal_error(format!("query findings: {e}"), None))?;
        }
    }

    // --- summarize ---
    let mut by_scanner: BTreeMap<String, i64> = BTreeMap::new();
    let mut by_severity: BTreeMap<String, i64> = BTreeMap::new();
    for r in &rows {
        *by_scanner.entry(r.scanner.clone()).or_default() += 1;
        *by_severity.entry(r.severity.clone()).or_default() += 1;
    }
    let findings: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "scanner": r.scanner,
                "severity": r.severity,
                "rule_id": r.rule_id,
                "file": r.file_path,
                "line": r.line,
                "title": r.title,
                "message": r.message,
                "status": r.status,
                "first_seen": r.first_seen_at,
                "last_seen": r.last_seen_at,
            })
        })
        .collect();

    json_result(&json!({
        "project": project,
        "refreshed": refresh,
        "scan": scan_report,
        "count": rows.len(),
        "by_scanner": by_scanner,
        "by_severity": by_severity,
        "findings": findings,
        "guidance": "Findings from installed external security scanners (gitleaks/semgrep/trivy/cargo-audit/…) over your indexed projects. Pass refresh=true to re-run the scanners now (subprocess sweep). High/critical findings become pending bug work items when a project sets [tracker] auto_promote_findings=true; run trigger_cron job=\"findings-promotion\" to promote, then triage them in the tracker.",
    }))
}
