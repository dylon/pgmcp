//! Queries for the external security-scanner subsystem
//! (`src/cron/security_scan.rs`): the `external_scanner_runs` audit trail, the
//! fingerprint-keyed `external_scanner_findings` table, and the
//! `external_scanner_sbom` artifacts. Schema: `v34_external_scanner_findings`.
#![allow(unused_imports)]

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;

/// A persisted scanner finding — the row shape the `security_scan` MCP tool and
/// the findings-promotion cron read back.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct ScannerFindingRow {
    pub id: i64,
    pub project_id: i32,
    pub scanner: String,
    pub rule_id: Option<String>,
    pub severity: String,
    pub file_path: Option<String>,
    pub line: Option<i32>,
    pub title: String,
    pub message: Option<String>,
    pub fingerprint: String,
    pub provenance_key: String,
    pub status: String,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

/// Distinct indexed languages for a project — the language-gating signal for the
/// scan engine (e.g. run `bandit` only where Python files were indexed).
pub async fn project_languages(
    pool: &PgPool,
    project_id: i32,
) -> Result<HashSet<String>, sqlx::Error> {
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT language FROM indexed_files
          WHERE project_id = $1 AND language IS NOT NULL",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Insert a per-(project, scanner) run-audit row, returning its id.
#[allow(clippy::too_many_arguments)]
pub async fn insert_scanner_run(
    pool: &PgPool,
    project_id: i32,
    scanner: &str,
    status: &str,
    exit_code: Option<i32>,
    duration_ms: i64,
    findings_count: i32,
    tool_version: Option<&str>,
    detail: Option<&str>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO external_scanner_runs
            (project_id, scanner, status, exit_code, duration_ms,
             findings_count, tool_version, detail, finished_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8, now())
         RETURNING id",
    )
    .bind(project_id)
    .bind(scanner)
    .bind(status)
    .bind(exit_code)
    .bind(duration_ms)
    .bind(findings_count)
    .bind(tool_version)
    .bind(detail)
    .fetch_one(pool)
    .await
}

/// Upsert a finding keyed on `fingerprint`. On conflict: refresh `last_seen_at`,
/// re-open (`status='open'`), relink the run, and update the mutable fields.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_scanner_finding(
    pool: &PgPool,
    project_id: i32,
    run_id: i64,
    scanner: &str,
    rule_id: Option<&str>,
    severity: &str,
    file_path: Option<&str>,
    line: Option<i32>,
    title: &str,
    message: Option<&str>,
    raw: &serde_json::Value,
    fingerprint: &str,
    provenance_key: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO external_scanner_findings
            (project_id, run_id, scanner, rule_id, severity, file_path, line,
             title, message, raw, fingerprint, provenance_key, status, last_seen_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,'open', now())
         ON CONFLICT (fingerprint) DO UPDATE SET
            run_id = EXCLUDED.run_id,
            severity = EXCLUDED.severity,
            rule_id = EXCLUDED.rule_id,
            title = EXCLUDED.title,
            message = EXCLUDED.message,
            raw = EXCLUDED.raw,
            status = 'open',
            last_seen_at = now()",
    )
    .bind(project_id)
    .bind(run_id)
    .bind(scanner)
    .bind(rule_id)
    .bind(severity)
    .bind(file_path)
    .bind(line)
    .bind(title)
    .bind(message)
    .bind(raw)
    .bind(fingerprint)
    .bind(provenance_key)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark every still-`open` finding for (project, scanner) whose fingerprint is
/// NOT in `seen` as `resolved`. An empty `seen` resolves all of that scanner's
/// open findings for the project (it ran and found nothing this pass).
pub async fn mark_unseen_resolved(
    pool: &PgPool,
    project_id: i32,
    scanner: &str,
    seen: &[String],
) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE external_scanner_findings
            SET status = 'resolved'
          WHERE project_id = $1 AND scanner = $2 AND status = 'open'
            AND NOT (fingerprint = ANY($3))",
    )
    .bind(project_id)
    .bind(scanner)
    .bind(seen)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Query findings with optional filters. `min_severity_rank` is a tracker
/// `Severity` rank floor (Critical=4 … Low=1; 0 = no floor). `status` filters
/// open/resolved (None = both). Ordered by severity desc, then recency.
pub async fn query_scanner_findings(
    pool: &PgPool,
    project_id: Option<i32>,
    scanners: Option<&[String]>,
    min_severity_rank: i32,
    status: Option<&str>,
    limit: i64,
) -> Result<Vec<ScannerFindingRow>, sqlx::Error> {
    let scanners_vec: Option<Vec<String>> = scanners.map(<[String]>::to_vec);
    sqlx::query_as::<_, ScannerFindingRow>(
        "SELECT id, project_id, scanner, rule_id, severity, file_path, line,
                title, message, fingerprint, provenance_key, status,
                first_seen_at, last_seen_at
           FROM external_scanner_findings
          WHERE ($1::int IS NULL OR project_id = $1)
            AND ($2::text[] IS NULL OR scanner = ANY($2))
            AND ($3::text IS NULL OR status = $3)
            AND (CASE severity
                    WHEN 'critical' THEN 4 WHEN 'high' THEN 3
                    WHEN 'medium' THEN 2 WHEN 'low' THEN 1 ELSE 0 END) >= $4
          ORDER BY (CASE severity
                    WHEN 'critical' THEN 4 WHEN 'high' THEN 3
                    WHEN 'medium' THEN 2 WHEN 'low' THEN 1 ELSE 0 END) DESC,
                   last_seen_at DESC
          LIMIT $5",
    )
    .bind(project_id)
    .bind(scanners_vec)
    .bind(status)
    .bind(min_severity_rank)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Aggregate counts by (scanner, severity) for the `security_scan` tool summary.
pub async fn scanner_finding_counts(
    pool: &PgPool,
    project_id: Option<i32>,
    status: Option<&str>,
) -> Result<Vec<(String, String, i64)>, sqlx::Error> {
    sqlx::query_as::<_, (String, String, i64)>(
        "SELECT scanner, severity, COUNT(*)::bigint
           FROM external_scanner_findings
          WHERE ($1::int IS NULL OR project_id = $1)
            AND ($2::text IS NULL OR status = $2)
          GROUP BY scanner, severity
          ORDER BY scanner, severity",
    )
    .bind(project_id)
    .bind(status)
    .fetch_all(pool)
    .await
}

/// Store (or refresh) the SBOM artifact a generator (`syft`) produced for a
/// project. One current SBOM per (project, scanner, format).
pub async fn upsert_scanner_sbom(
    pool: &PgPool,
    project_id: i32,
    scanner: &str,
    format: &str,
    sbom: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO external_scanner_sbom (project_id, scanner, format, sbom, generated_at)
         VALUES ($1,$2,$3,$4, now())
         ON CONFLICT (project_id, scanner, format)
         DO UPDATE SET sbom = EXCLUDED.sbom, generated_at = now()",
    )
    .bind(project_id)
    .bind(scanner)
    .bind(format)
    .bind(sbom)
    .execute(pool)
    .await?;
    Ok(())
}
