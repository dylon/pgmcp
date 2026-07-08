//! Code-health collectors: complexity, defect-proneness, tech debt, signature
//! smells, refactor clusters.

use rmcp::ErrorData as McpError;
use serde_json::json;

use super::{DEBT_MARKER_PATTERN, truncate_preview};
use crate::context::SystemContext;
use crate::mcp::tools::sota_helpers::pool_or_err;
use crate::quality::findings::{Finding, FindingCategory, Severity};

const CH: FindingCategory = FindingCategory::CodeHealth;

/// Composite complexity per file — normalized worst-function cyclomatic, size,
/// and coupling. Mirrors `complexity_hotspots`' signal off `file_metrics` +
/// `function_metrics`.
pub async fn collect_complexity_hotspots(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        relative_path: String,
        line_count: i32,
        coupling: i64,
        cyc_max: i64,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT f.relative_path, f.line_count,
                (COALESCE(fm.afferent_coupling,0) + COALESCE(fm.efferent_coupling,0))::BIGINT AS coupling,
                COALESCE(mc.cyc,0)::BIGINT AS cyc_max
         FROM indexed_files f
         LEFT JOIN file_metrics fm ON fm.file_id = f.id
         LEFT JOIN (SELECT file_id, MAX(cyclomatic) cyc FROM function_metrics GROUP BY file_id) mc
                ON mc.file_id = f.id
         WHERE f.project_id = $1",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("complexity query failed: {e}"), None))?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        // Absolute, criterion-referenced severity (McCabe cyclomatic bands +
        // file-size bands), NOT normalized against the project's own max — a
        // codebase must not be graded against itself (no curve), and only
        // genuine outliers are findings. Files below the Medium bar are healthy
        // and produce NO finding (this is what stops the collector flagging all
        // 814 files and saturating finding_density to 0). cyc_max contributes
        // once function_metrics is populated; line_count is the always-available
        // absolute signal in the meantime.
        let severity = if r.cyc_max >= 20 || r.line_count >= 1000 {
            Severity::High
        } else if r.cyc_max >= 10 || r.line_count >= 500 {
            Severity::Medium
        } else {
            continue;
        };
        // Descriptive composite against FIXED ceilings (absolute) — used only to
        // rank the surfaced outliers, never to assign severity.
        let composite = ((r.cyc_max as f64 / 30.0)
            + (r.line_count as f64 / 2000.0)
            + (r.coupling as f64 / 60.0))
            / 3.0;
        out.push(
            Finding::new(
                "complexity_hotspots",
                CH,
                project_name,
                severity,
                format!(
                    "{} — composite complexity {:.2} (max cyclomatic {}, {} lines, coupling {})",
                    r.relative_path, composite, r.cyc_max, r.line_count, r.coupling
                ),
            )
            .with_score(composite)
            .at_file(&r.relative_path)
            .with_kind("complexity_hotspot")
            .with_raw(json!({
                "path": r.relative_path, "line_count": r.line_count,
                "coupling": r.coupling, "cyclomatic_max": r.cyc_max,
                "composite_score": format!("{composite:.4}"),
            })),
        );
    }
    Ok(out)
}

/// Defect-proneness per file via the hand-weighted heuristic (the trained model
/// the tool fits is out of scope for the lean collector). Mirrors
/// `bug_prediction`'s `file_metrics` features.
pub async fn collect_bug_prediction(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        relative_path: String,
        line_count: i32,
        churn_rate: Option<f64>,
        fix_commit_ratio: Option<f64>,
        author_count: Option<i32>,
        in_degree: Option<i32>,
        out_degree: Option<i32>,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT f.relative_path, f.line_count, fm.churn_rate, fm.fix_commit_ratio,
                fm.author_count, fm.in_degree, fm.out_degree
         FROM indexed_files f
         JOIN file_metrics fm ON fm.file_id = f.id
         WHERE f.project_id = $1",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("bug_prediction query failed: {e}"), None))?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let churn = r.churn_rate.unwrap_or(0.0);
        let fix = r.fix_commit_ratio.unwrap_or(0.0);
        let coupling = (r.in_degree.unwrap_or(0) + r.out_degree.unwrap_or(0)) as f64;
        let size = (r.line_count as f64 / 100.0).min(10.0);
        let authors = r.author_count.unwrap_or(1) as f64;
        let bug_score = (churn * 0.3
            + fix * 3.0
            + size * 0.2
            + coupling * 0.05
            + (authors - 1.0).max(0.0) * 0.1)
            .max(0.0);
        // Absolute defect-proneness bands: only files that cross the Medium bar
        // are findings. The previous `>= 0.05` floor emitted a Low finding for
        // almost every file (780/814), which — summed by finding_density — was a
        // primary cause of the saturated 0.0 grade. Healthy files are not
        // findings; this is criterion-referenced, not a per-project curve.
        let severity = match bug_score {
            s if s >= 0.7 => Severity::High,
            s if s >= 0.4 => Severity::Medium,
            _ => continue,
        };
        out.push(
            Finding::new(
                "bug_prediction",
                CH,
                project_name,
                severity,
                format!(
                    "{} — defect-proneness {:.2} (churn {:.2}, fix-ratio {:.2}, coupling {})",
                    r.relative_path, bug_score, churn, fix, coupling as i64
                ),
            )
            .with_score(bug_score)
            .at_file(&r.relative_path)
            .with_kind("bug_prone")
            .with_raw(json!({
                "path": r.relative_path, "bug_score": format!("{bug_score:.4}"),
                "churn_rate": churn, "fix_ratio": fix, "coupling": coupling as i64,
            })),
        );
    }
    Ok(out)
}

/// Per-file technical-debt composite (TODO density + cyclomatic proxy + churn +
/// fix ratio). Mirrors `technical_debt_analysis`.
pub async fn collect_technical_debt(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        relative_path: String,
        line_count: i32,
        content: Option<String>,
        churn_rate: Option<f64>,
        fix_commit_ratio: Option<f64>,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT f.relative_path, f.line_count, f.content, fm.churn_rate, fm.fix_commit_ratio
         FROM indexed_files f
         LEFT JOIN file_metrics fm ON fm.file_id = f.id
         WHERE f.project_id = $1 AND f.content IS NOT NULL",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("technical_debt query failed: {e}"), None))?;

    let todo_re = regex::Regex::new(DEBT_MARKER_PATTERN).expect("valid regex");
    let branch_re =
        regex::Regex::new(r"(?m)^\s*(if|else\s+if|elif|for|while|match|case|catch|except)\b")
            .expect("valid regex");

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let content = r.content.as_deref().unwrap_or("");
        let todo_count = todo_re.find_iter(content).count();
        let todo_density = if r.line_count > 0 {
            todo_count as f64 / r.line_count as f64 * 1000.0
        } else {
            0.0
        };
        let branches = branch_re.find_iter(content).count();
        let complexity_factor = ((branches as f64 + 1.0) / 20.0).min(1.0);
        let churn = r.churn_rate.unwrap_or(0.0).min(10.0) / 10.0;
        let fix = r.fix_commit_ratio.unwrap_or(0.0);
        let debt = todo_density * 0.3
            + complexity_factor * 0.25
            + churn * 0.2
            + fix * 0.15
            + (r.line_count as f64 / 1000.0).min(1.0) * 0.1;
        if debt < 0.05 {
            continue;
        }
        // debt is a 0..~1 composite of fractions; calibrate on that scale (the
        // plan's 50/20 cutoffs were mis-scaled for this 0..1 metric).
        let severity = match debt {
            d if d >= 0.5 => Severity::High,
            d if d >= 0.2 => Severity::Medium,
            _ => Severity::Low,
        };
        out.push(
            Finding::new(
                "technical_debt_analysis",
                CH,
                project_name,
                severity,
                format!(
                    "{} — debt score {:.2} ({} markers, cyclomatic ~{})",
                    r.relative_path,
                    debt,
                    todo_count,
                    branches + 1
                ),
            )
            .with_score(debt)
            .at_file(&r.relative_path)
            .with_kind("tech_debt")
            .with_raw(json!({
                "path": r.relative_path, "debt_score": format!("{debt:.4}"),
                "todo_count": todo_count, "cyclomatic_complexity": branches + 1,
            })),
        );
    }
    Ok(out)
}

/// Per-line debt-marker comments (TODO/FIXME/HACK/…). Mirrors
/// `documented_tech_debt`'s marker scan; severity by marker class.
pub async fn collect_documented_tech_debt(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    // Corpus-scale content scan — routed through the shared PG-timeout-lifted
    // loader so a large project's read is not cancelled at the pool's 30 s default.
    let rows = super::load_project_file_contents(pool, project_id, None).await?;

    let marker_re =
        regex::Regex::new(r"(?i)\b(TODO|FIXME|HACK|XXX|BUG|WORKAROUND)\b").expect("valid regex");
    let mut out = Vec::new();
    for (relative_path, content) in &rows {
        let content = content.as_deref().unwrap_or("");
        for (i, line) in content.lines().enumerate() {
            if let Some(m) = marker_re.find(line) {
                let marker = m.as_str().to_ascii_uppercase();
                let severity = match marker.as_str() {
                    "FIXME" | "HACK" | "BUG" | "XXX" => Severity::Medium,
                    _ => Severity::Low,
                };
                out.push(
                    Finding::new(
                        "documented_tech_debt",
                        CH,
                        project_name,
                        severity,
                        format!("{marker}: {}", truncate_preview(line, 100)),
                    )
                    .at(relative_path, (i + 1) as u32)
                    .with_kind(format!("marker_{}", marker.to_ascii_lowercase()))
                    .with_raw(json!({
                        "file": relative_path, "line": i + 1, "kind": marker,
                        "snippet": truncate_preview(line, 200),
                    })),
                );
            }
        }
    }
    Ok(out)
}

/// Signature smells — long parameter lists and boolean-flag explosion. Mirrors
/// `signature_lint` off `file_symbols` + `symbol_parameters`.
pub async fn collect_signature_lint(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        relative_path: String,
        name: String,
        start_line: i32,
        param_count: i64,
        bool_count: i64,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT f.relative_path, fs.name, fs.start_line,
                COUNT(sp.id) FILTER (WHERE NOT sp.is_self) AS param_count,
                COUNT(sp.id) FILTER (WHERE sp.type_raw ILIKE 'bool%' OR 'bool' = ANY(sp.type_tags)) AS bool_count
         FROM file_symbols fs
         JOIN indexed_files f ON f.id = fs.file_id
         LEFT JOIN symbol_parameters sp ON sp.symbol_id = fs.id
         WHERE f.project_id = $1 AND fs.kind IN ('function','method')
         GROUP BY f.relative_path, fs.name, fs.start_line
         HAVING COUNT(sp.id) FILTER (WHERE NOT sp.is_self) > 5
             OR COUNT(sp.id) FILTER (WHERE sp.type_raw ILIKE 'bool%' OR 'bool' = ANY(sp.type_tags)) >= 2",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("signature_lint query failed: {e}"), None))?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let (kind, severity, what) = if r.bool_count >= 2 {
            (
                "boolean_flag_explosion",
                Severity::Medium,
                format!("{} boolean flag params", r.bool_count),
            )
        } else {
            (
                "long_parameter_list",
                Severity::Low,
                format!("{} parameters", r.param_count),
            )
        };
        out.push(
            Finding::new(
                "signature_lint",
                CH,
                project_name,
                severity,
                format!("{}() — {what}", r.name),
            )
            .at(&r.relative_path, r.start_line.max(0) as u32)
            .with_kind(kind)
            .with_raw(json!({
                "file": r.relative_path, "symbol": r.name, "line": r.start_line,
                "param_count": r.param_count, "bool_count": r.bool_count,
            })),
        );
    }
    Ok(out)
}

/// Cross-project duplicate clusters this project participates in — extraction
/// candidates. Mirrors `refactoring_report` off `cross_project_similarities`.
/// Cluster-keyed: the discriminator lives in `kind`.
pub async fn collect_refactoring_report(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        path_a: String,
        project_name_a: String,
        path_b: String,
        project_name_b: String,
        chunk_similarity: f64,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT path_a, project_name_a, path_b, project_name_b, chunk_similarity
         FROM cross_project_similarities
         WHERE (project_id_a = $1 OR project_id_b = $1) AND chunk_similarity >= 0.85
         ORDER BY chunk_similarity DESC
         LIMIT 200",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("refactoring_report query failed: {e}"), None))?;

    let mut out = Vec::with_capacity(rows.len());
    for (i, r) in rows.iter().enumerate() {
        // Present from the perspective of the queried project.
        let (this_path, other_path, other_proj) = if r.project_name_a == project_name {
            (&r.path_a, &r.path_b, &r.project_name_b)
        } else {
            (&r.path_b, &r.path_a, &r.project_name_a)
        };
        out.push(
            Finding::new(
                "refactoring_report",
                CH,
                project_name,
                Severity::Low,
                format!(
                    "{} duplicates {}:{} ({:.0}% similar) — extraction candidate",
                    this_path,
                    other_proj,
                    other_path,
                    r.chunk_similarity * 100.0
                ),
            )
            .with_score(r.chunk_similarity)
            .at_file(this_path)
            .with_kind(format!("refactor_pair:{i}"))
            .with_raw(json!({
                "path": this_path, "duplicate_of": other_path,
                "other_project": other_proj, "similarity": r.chunk_similarity,
            })),
        );
    }
    Ok(out)
}
