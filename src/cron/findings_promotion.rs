//! `findings-promotion` cron: idempotently materialize high-signal analytic
//! findings into `pending` work items so they surface in the tracker / digest
//! instead of dying as JSON behind a tool call.
//!
//! Two finding sources (the closed [`crate::tracker::git_link::FindingSource`]):
//!   - `bug_prediction` — files whose defect-proneness score ≥ the per-project
//!     threshold become `pending` `bug` items;
//!   - `documented_tech_debt` — high-severity comment markers (FIXME/BUG/HACK/
//!     KLUDGE/WTF/XXX) become `pending` `fixme` items.
//!
//! The scan + scoring are the SHARED primitives in
//! [`crate::code_analysis::findings`], so the cron promotes exactly what the
//! `bug_prediction` / `documented_tech_debt` tools report.
//!
//! IDEMPOTENCY: each finding has a stable `provenance_key`; promotion goes
//! through [`crate::db::queries::promote_finding`], which is a no-op (returns the
//! existing item, `created=false`) when the key already exists. Running the cron
//! twice yields exactly one item per finding.
//!
//! TRUST: promoted items land in `pending` — NEVER pre-`confirmed` (confirmation
//! is user-only). The cron performs no status transitions.
//!
//! OPT-IN: per-project, default OFF (`[tracker] auto_promote_findings = true` in
//! the project's `.pgmcp.toml`). The cron skips every project that has not
//! opted in. Light job (bounded per-project queries) — no heavy-cron gate.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{error, info};

use crate::code_analysis::findings::{self, BugFeatures};
use crate::config::ProjectOverride;
use crate::db::queries::{self, FindingAnchor, NewWorkItem};
use crate::stats::tracker::StatsTracker;
use crate::tracker::git_link::FindingSource;

/// Max items promoted per (project, source) per run — a guardrail so a project
/// that first opts in with thousands of TODO markers does not flood the tracker
/// in one sweep (the rest are picked up on subsequent runs).
const MAX_PROMOTIONS_PER_SOURCE: usize = 50;

/// One findings-promotion sweep across all opted-in projects. `pool` is an owned
/// `PgPool` (cheaply cloned from `DbClient::pool()`). Best-effort per project:
/// one project's failure never aborts the sweep.
pub async fn run_or_log(pool: PgPool, stats: Arc<StatsTracker>) {
    let _ = stats.cron_executions.fetch_add(1, Ordering::Relaxed);
    let projects = match queries::list_projects(&pool).await {
        Ok(p) => p,
        Err(e) => {
            stats.cron_panics.fetch_add(1, Ordering::Relaxed);
            error!(error = %e, "findings-promotion: list_projects failed");
            return;
        }
    };

    let mut promoted_total = 0u64;
    for project in &projects {
        // Per-project opt-in gate (default OFF).
        let Some(over) = ProjectOverride::load(Path::new(&project.path)).and_then(|o| o.tracker)
        else {
            continue;
        };
        if !over.auto_promote_findings {
            continue;
        }
        let threshold = over.findings_bug_score_threshold;

        match promote_for_project(&pool, project.id, &project.name, threshold).await {
            Ok(n) => promoted_total += n,
            Err(e) => {
                error!(
                    error = %e,
                    project = %project.name,
                    "findings-promotion: project sweep failed (non-fatal)"
                );
            }
        }
    }

    if promoted_total > 0 {
        stats
            .findings_promoted
            .fetch_add(promoted_total, Ordering::Relaxed);
        info!(
            promoted = promoted_total,
            "findings-promotion: materialized new pending work items"
        );
    }
}

/// Promote both finding sources for one project. Returns the count of
/// newly-created items (re-promotions of already-known findings count 0).
async fn promote_for_project(
    pool: &PgPool,
    project_id: i32,
    project_name: &str,
    bug_score_threshold: f64,
) -> Result<u64, sqlx::Error> {
    let mut created = 0u64;
    created += promote_bug_prediction(pool, project_id, project_name, bug_score_threshold).await?;
    created += promote_documented_tech_debt(pool, project_id, project_name).await?;
    created += promote_security_scan(pool, project_id).await?;
    Ok(created)
}

/// File-level `bug_prediction` findings ≥ threshold → `pending` `bug` items.
async fn promote_bug_prediction(
    pool: &PgPool,
    project_id: i32,
    project_name: &str,
    threshold: f64,
) -> Result<u64, sqlx::Error> {
    // (file_id, relative_path, language, line_count, churn, fix_ratio,
    //  commits, authors, in_deg, out_deg) — the same join bug_prediction uses,
    //  plus f.id for the code anchor.
    #[derive(sqlx::FromRow)]
    struct Row {
        file_id: i64,
        relative_path: String,
        language: String,
        line_count: i32,
        churn_rate: Option<f64>,
        fix_commit_ratio: Option<f64>,
        commit_count: Option<i32>,
        author_count: Option<i32>,
        in_degree: Option<i32>,
        out_degree: Option<i32>,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT f.id AS file_id, f.relative_path, f.language, f.line_count,
                fm.churn_rate, fm.fix_commit_ratio, fm.commit_count,
                fm.author_count, fm.in_degree, fm.out_degree
         FROM indexed_files f
         JOIN file_metrics fm ON fm.file_id = f.id
         WHERE f.project_id = $1",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(0);
    }

    // file_id lookup by path (the scorer works on BugFeatures, which omit it).
    let file_id_by_path: std::collections::HashMap<&str, i64> = rows
        .iter()
        .map(|r| (r.relative_path.as_str(), r.file_id))
        .collect();
    let features: Vec<BugFeatures> = rows
        .iter()
        .map(|r| BugFeatures {
            relative_path: r.relative_path.clone(),
            language: r.language.clone(),
            line_count: r.line_count,
            churn_rate: r.churn_rate,
            fix_commit_ratio: r.fix_commit_ratio,
            commit_count: r.commit_count,
            author_count: r.author_count,
            in_degree: r.in_degree,
            out_degree: r.out_degree,
        })
        .collect();
    let (scored, score_kind) = findings::score_bug_files(&features);

    let mut created = 0u64;
    // `scored` is sorted descending, so `take_while(score ≥ threshold)` stops at
    // the first below-threshold file, and `take(MAX)` caps promotions per run.
    for s in scored
        .iter()
        .take_while(|s| s.bug_score >= threshold)
        .take(MAX_PROMOTIONS_PER_SOURCE)
    {
        // Stable provenance key: source + project + path (NOT the score, which
        // drifts run-to-run — keying on it would re-promote on every tick).
        let provenance_key = format!(
            "{}:{}:{}",
            FindingSource::BugPrediction.as_str(),
            project_name,
            s.relative_path
        );
        let title = format!("Bug-prone file: {}", s.relative_path);
        let body = format!(
            "Auto-promoted by the findings-promotion cron from `bug_prediction` \
             (score {:.4}, {}). This file scores at or above the defect-proneness \
             threshold ({:.2}); review and add tests / refactor.\n\nFile: {}",
            s.bug_score,
            score_kind.as_str(),
            threshold,
            s.relative_path
        );
        let public_id = gen_finding_public_id("bug");
        let item = NewWorkItem {
            public_id: &public_id,
            project_id: Some(project_id),
            // bug_prediction → a `bug` kind, born `pending` (NOT confirmed —
            // confirmation is user-only). priority seeded modestly.
            kind: FindingSource::BugPrediction.item_kind(),
            status: "pending",
            title: &title,
            body: Some(&body),
            priority: 30,
            origin: "agent_write",
            ..Default::default()
        };
        let anchor = FindingAnchor {
            file_id: file_id_by_path.get(s.relative_path.as_str()).copied(),
            ..Default::default()
        };
        match queries::promote_finding(
            pool,
            &provenance_key,
            FindingSource::BugPrediction.as_str(),
            item,
            anchor,
        )
        .await
        {
            Ok((_, was_created)) => {
                if was_created {
                    created += 1;
                }
            }
            Err(e) => error!(
                error = ?e,
                path = %s.relative_path,
                "findings-promotion: promote bug_prediction finding failed"
            ),
        }
    }
    Ok(created)
}

/// High-severity `documented_tech_debt` markers → `pending` `fixme` items.
async fn promote_documented_tech_debt(
    pool: &PgPool,
    project_id: i32,
    project_name: &str,
) -> Result<u64, sqlx::Error> {
    // Files with content for this project (same selection documented_tech_debt
    // uses, minus the glob excludes — the cron is a coarse promoter, not the
    // full tool surface).
    let files: Vec<(i64, String, Option<String>)> =
        sqlx::query_as::<_, (i64, String, Option<String>)>(
            "SELECT f.id, f.relative_path, f.content
             FROM indexed_files f
             WHERE f.project_id = $1 AND f.content IS NOT NULL",
        )
        .bind(project_id)
        .fetch_all(pool)
        .await?;

    let mut created = 0u64;
    let mut promoted = 0usize;
    'files: for (file_id, path, content_opt) in &files {
        let Some(content) = content_opt else { continue };
        for hit in findings::scan_comment_markers(content, &["high"]) {
            if promoted >= MAX_PROMOTIONS_PER_SOURCE {
                break 'files;
            }
            promoted += 1;

            // Provenance key: source + project + path + line + marker kind, so
            // each distinct marker promotes once and survives re-scans.
            let provenance_key = format!(
                "{}:{}:{}:{}:{}",
                FindingSource::DocumentedTechDebt.as_str(),
                project_name,
                path,
                hit.line,
                hit.kind
            );
            let title = format!("{} at {}:{}", hit.kind, path, hit.line);
            let body = format!(
                "Auto-promoted by the findings-promotion cron from `documented_tech_debt` \
                 (high-severity marker).\n\n{}:{}\n\n    {}",
                path, hit.line, hit.snippet
            );
            let public_id = gen_finding_public_id("fixme");
            let item = NewWorkItem {
                public_id: &public_id,
                project_id: Some(project_id),
                // documented_tech_debt → a `fixme` (lightweight marker) kind,
                // born `pending`.
                kind: FindingSource::DocumentedTechDebt.item_kind(),
                status: "pending",
                title: &title,
                body: Some(&body),
                priority: 20,
                origin: "agent_write",
                ..Default::default()
            };
            let anchor = FindingAnchor {
                file_id: Some(*file_id),
                ..Default::default()
            };
            match queries::promote_finding(
                pool,
                &provenance_key,
                FindingSource::DocumentedTechDebt.as_str(),
                item,
                anchor,
            )
            .await
            {
                Ok((_, was_created)) => {
                    if was_created {
                        created += 1;
                    }
                }
                Err(e) => error!(
                    error = ?e,
                    path = %path,
                    line = hit.line,
                    "findings-promotion: promote tech-debt finding failed"
                ),
            }
        }
    }
    Ok(created)
}

/// High-severity (critical/high) open external-scanner findings → `pending`
/// `bug` items (the `security_scan` source). Idempotent on each finding's own
/// `provenance_key` (`security_scan:<scanner>:<sha256>`), stored in
/// `external_scanner_findings` by the `security_scan` cron / tool.
async fn promote_security_scan(pool: &PgPool, project_id: i32) -> Result<u64, sqlx::Error> {
    // Rank floor 3 = High (covers critical + high), open findings only.
    let findings = queries::query_scanner_findings(
        pool,
        Some(project_id),
        None,
        3,
        Some("open"),
        MAX_PROMOTIONS_PER_SOURCE as i64,
        Some("security"),
    )
    .await?;

    let mut created = 0u64;
    for f in &findings {
        let location = match (&f.file_path, f.line) {
            (Some(p), Some(l)) => format!("{p}:{l}"),
            (Some(p), None) => p.clone(),
            _ => "—".to_string(),
        };
        let title = format!("[{}] {}", f.scanner, f.title);
        let body = format!(
            "Auto-promoted by the findings-promotion cron from the `{}` scanner \
             ({} severity).\n\nRule: {}\nLocation: {}\n\n{}",
            f.scanner,
            f.severity,
            f.rule_id.as_deref().unwrap_or("—"),
            location,
            f.message.as_deref().unwrap_or(""),
        );
        let priority = match f.severity.as_str() {
            "critical" => 90,
            "high" => 70,
            "medium" => 40,
            _ => 20,
        };
        let public_id = gen_finding_public_id("sec");
        let file_id = match &f.file_path {
            Some(p) => resolve_file_id(pool, project_id, p).await,
            None => None,
        };
        let item = NewWorkItem {
            public_id: &public_id,
            project_id: Some(project_id),
            // External security findings are first-class `bug`s, born `pending`
            // (NOT confirmed — confirmation is user-only). severity carried over.
            kind: FindingSource::SecurityScan.item_kind(),
            status: "pending",
            title: &title,
            body: Some(&body),
            priority,
            origin: "agent_write",
            severity: Some(f.severity.as_str()),
            ..Default::default()
        };
        let anchor = FindingAnchor {
            file_id,
            ..Default::default()
        };
        match queries::promote_finding(
            pool,
            &f.provenance_key,
            FindingSource::SecurityScan.as_str(),
            item,
            anchor,
        )
        .await
        {
            Ok((item_id, was_created)) => {
                if was_created {
                    created += 1;
                    // Structured bug-detail sidecar: a scanner finding carries a
                    // real actual-behavior (its message), environment (scanner +
                    // severity), and root cause (the rule that fired). Only on
                    // first creation, so a later re-promotion never clobbers any
                    // user edits to the sidecar.
                    let environment = format!("scanner: {}; severity: {}", f.scanner, f.severity);
                    let details = queries::BugDetailFields {
                        actual_behavior: f.message.as_deref(),
                        environment: Some(&environment),
                        root_cause: f.rule_id.as_deref(),
                        reported_by: Some("security_scan"),
                        ..Default::default()
                    };
                    if let Err(e) = queries::upsert_bug_details(pool, item_id, &details).await {
                        error!(
                            error = ?e,
                            scanner = %f.scanner,
                            "findings-promotion: security bug_details upsert failed"
                        );
                    }
                }
            }
            Err(e) => error!(
                error = ?e,
                scanner = %f.scanner,
                "findings-promotion: promote security_scan finding failed"
            ),
        }
    }
    Ok(created)
}

/// Best-effort resolution of a scanner-reported path to an `indexed_files.id`,
/// so a promoted item carries a code anchor. `None` when the path isn't indexed
/// (e.g. a synthetic `Cargo.lock` target or an absolute path outside the index).
async fn resolve_file_id(pool: &PgPool, project_id: i32, path: &str) -> Option<i64> {
    let rel = path.trim_start_matches("./");
    sqlx::query_scalar::<_, i64>(
        "SELECT id FROM indexed_files WHERE project_id = $1 AND relative_path = $2 LIMIT 1",
    )
    .bind(project_id)
    .bind(rel)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// A stable, human-legible `public_id` for a promoted finding: a fixed prefix
/// plus a short random suffix (the provenance ledger, not this id, is what
/// guarantees idempotency — so a fresh suffix per promotion attempt is fine; a
/// re-promotion never reaches the insert path).
fn gen_finding_public_id(prefix: &str) -> String {
    format!(
        "finding-{}-{}",
        prefix,
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    )
}
