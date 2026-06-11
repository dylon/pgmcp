//! Hygiene collectors: topic orphans, unreferenced symbols, stale zombies,
//! metric anomalies, naming inconsistency.
//!
//! `dead_columns` (needs SQL-schema column-usage analysis) and
//! `embedding_outliers` (needs LOF over chunk embedding vectors) are not
//! collected here — neither has a materialized signal cheap to query — so the
//! aggregator omits them rather than emit a faked one.

use std::collections::HashMap;

use rmcp::ErrorData as McpError;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::tools::sota_helpers::pool_or_err;
use crate::quality::findings::{Finding, FindingCategory, Severity};

const HY: FindingCategory = FindingCategory::Hygiene;

/// Files whose chunks have only weak topic membership — semantic orphans.
/// Empty until the topic-clustering cron has run (inner join yields nothing).
pub async fn collect_find_orphans(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        relative_path: String,
        max_mem: f64,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT f.relative_path, MAX(cta.membership_score)::DOUBLE PRECISION AS max_mem
         FROM indexed_files f
         JOIN file_chunks fc ON fc.file_id = f.id
         JOIN chunk_topic_assignments cta ON cta.chunk_id = fc.id
         WHERE f.project_id = $1
         GROUP BY f.relative_path
         HAVING MAX(cta.membership_score) < 0.15",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("find_orphans query failed: {e}"), None))?;

    Ok(rows
        .into_iter()
        .map(|r| {
            Finding::new(
                "find_orphans",
                HY,
                project_name,
                Severity::Low,
                format!(
                    "{} has weak topic membership ({:.2}) — semantic orphan",
                    r.relative_path, r.max_mem
                ),
            )
            .with_score(r.max_mem)
            .at_file(&r.relative_path)
            .with_kind("orphan")
            .with_raw(json!({ "path": r.relative_path, "max_membership": r.max_mem }))
        })
        .collect())
}

/// Functions/methods with no incoming references. Private unreferenced symbols
/// are likely dead (Medium); public ones may be external API (Low, higher
/// false-positive rate since callers can live outside the index).
pub async fn collect_dead_code_reachability(
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
        visibility: Option<String>,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT f.relative_path, fs.name, fs.start_line, fs.visibility
         FROM file_symbols fs
         JOIN indexed_files f ON f.id = fs.file_id
         LEFT JOIN symbol_references sr ON sr.target_symbol_id = fs.id
         WHERE f.project_id = $1 AND fs.kind IN ('function','method')
           AND sr.id IS NULL
           AND fs.name NOT IN ('main','new','default','fmt','from','into','drop','clone','eq','hash')",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("dead_code query failed: {e}"), None))?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let is_pub = matches!(r.visibility.as_deref(), Some("pub") | Some("public"));
            let severity = if is_pub {
                Severity::Low
            } else {
                Severity::Medium
            };
            Finding::new(
                "dead_code_reachability",
                HY,
                project_name,
                severity,
                format!(
                    "{}() has no incoming references{}",
                    r.name,
                    if is_pub {
                        " (public — may have external callers)"
                    } else {
                        ""
                    }
                ),
            )
            .at(&r.relative_path, r.start_line.max(0) as u32)
            .with_kind("unreferenced_symbol")
            .with_raw(
                json!({ "file": r.relative_path, "name": r.name, "visibility": r.visibility }),
            )
        })
        .collect())
}

/// Old, unreferenced files — stale zombies.
pub async fn collect_stale_zombie(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        relative_path: String,
        days: i32,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT f.relative_path, fm.days_since_last_change AS days
         FROM indexed_files f JOIN file_metrics fm ON fm.file_id = f.id
         WHERE f.project_id = $1 AND fm.days_since_last_change > 365
           AND COALESCE(fm.in_degree,0) = 0
         ORDER BY fm.days_since_last_change DESC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("stale_zombie query failed: {e}"), None))?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let severity = if r.days > 730 {
                Severity::Medium
            } else {
                Severity::Low
            };
            Finding::new(
                "stale_zombie",
                HY,
                project_name,
                severity,
                format!(
                    "{} untouched {} days and depended-on by nothing — stale zombie",
                    r.relative_path, r.days
                ),
            )
            .with_score(r.days as f64)
            .at_file(&r.relative_path)
            .with_kind("stale_zombie")
            .with_raw(json!({ "path": r.relative_path, "days_idle": r.days }))
        })
        .collect())
}

/// Statistical size outliers — files whose line count is far above the project
/// mean (z-score over the file population).
pub async fn collect_anomaly_detection(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        relative_path: String,
        line_count: i32,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT relative_path, line_count FROM indexed_files WHERE project_id = $1 AND line_count > 0",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("anomaly_detection query failed: {e}"), None))?;

    if rows.len() < 8 {
        return Ok(Vec::new()); // too few files for a meaningful distribution
    }
    let n = rows.len() as f64;
    let mean = rows.iter().map(|r| r.line_count as f64).sum::<f64>() / n;
    let var = rows
        .iter()
        .map(|r| (r.line_count as f64 - mean).powi(2))
        .sum::<f64>()
        / n;
    let std = var.sqrt().max(1e-9);

    let mut out = Vec::new();
    for r in &rows {
        let z = (r.line_count as f64 - mean) / std;
        if z >= 2.5 {
            let severity = if z >= 4.0 {
                Severity::Medium
            } else {
                Severity::Low
            };
            out.push(
                Finding::new(
                    "anomaly_detection",
                    HY,
                    project_name,
                    severity,
                    format!(
                        "{} is a size outlier ({} lines, z={:.1})",
                        r.relative_path, r.line_count, z
                    ),
                )
                .with_score(z)
                .at_file(&r.relative_path)
                .with_kind("size_outlier")
                .with_raw(
                    json!({ "path": r.relative_path, "line_count": r.line_count, "z_score": z }),
                ),
            );
        }
    }
    Ok(out)
}

/// Function names that deviate from their file's dominant case convention.
pub async fn collect_naming_consistency(
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
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT f.relative_path, fs.name, fs.start_line
         FROM file_symbols fs JOIN indexed_files f ON f.id = fs.file_id
         WHERE f.project_id = $1 AND fs.kind IN ('function','method')",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("naming_consistency query failed: {e}"), None))?;

    // Group by file, classify each name, flag minority-style names.
    let mut by_file: HashMap<String, Vec<(String, i32)>> = HashMap::new();
    for r in rows {
        by_file
            .entry(r.relative_path)
            .or_default()
            .push((r.name, r.start_line));
    }

    let mut out = Vec::new();
    for (path, fns) in &by_file {
        if fns.len() < 4 {
            continue;
        }
        let mut snake = 0usize;
        let mut camel = 0usize;
        for (name, _) in fns {
            match classify_case(name) {
                Case::Snake => snake += 1,
                Case::Camel => camel += 1,
                Case::Other => {}
            }
        }
        if snake == 0 || camel == 0 {
            continue; // consistent (or unclassifiable)
        }
        let (dominant, dom_label) = if snake >= camel {
            (Case::Snake, "snake_case")
        } else {
            (Case::Camel, "camelCase")
        };
        for (name, line) in fns {
            let c = classify_case(name);
            if c != Case::Other && c != dominant {
                out.push(
                    Finding::new(
                        "naming_consistency",
                        HY,
                        project_name,
                        Severity::Low,
                        format!("{name}() breaks the file's dominant {dom_label} convention"),
                    )
                    .at(path, (*line).max(0) as u32)
                    .with_kind("naming_inconsistency")
                    .with_raw(json!({ "file": path, "symbol": name, "dominant": dom_label })),
                );
            }
        }
    }
    Ok(out)
}

/// Imports embedded inside a function/method/lambda body instead of at the top of
/// the file or module (a `mod tests { … }` top is fine — it resolves to the module,
/// not a callable). Pure shadow-ASR analysis: `nested_import_violations` joins each
/// `import_use` row to its resolved enclosing symbol and keeps only callable
/// enclosers. Severity rides `dup_count` — the same import re-typed across several
/// function bodies is the strongest "hoist me to the top" signal (and the
/// duplication that hoisting eliminates). Cross-language by design (every backend
/// language); empty until the symbol-extraction cron has run.
pub async fn collect_import_hygiene(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    let rows = crate::db::queries::nested_import_violations(pool, project_id, None)
        .await
        .map_err(|e| McpError::internal_error(format!("import_hygiene query failed: {e}"), None))?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let severity = match r.dup_count {
                n if n >= 4 => Severity::High,
                n if n >= 2 => Severity::Medium,
                _ => Severity::Low,
            };
            let mut description = format!(
                "`{}` is imported inside {} `{}` — move it to the file/module top",
                r.target_raw, r.enclosing_kind, r.enclosing_name
            );
            if r.dup_count >= 2 {
                description.push_str(&format!(
                    " (appears in {} function bodies in this file)",
                    r.dup_count
                ));
            }
            Finding::new("import_hygiene", HY, project_name, severity, description)
                .at(&r.relative_path, r.source_line.max(0) as u32)
                .with_score(r.dup_count as f64)
                .with_kind("nested_import")
                .with_raw(json!({
                    "file": r.relative_path,
                    "line": r.source_line,
                    "import": r.target_raw,
                    "in_symbol": r.enclosing_name,
                    "in_kind": r.enclosing_kind,
                    "dup_count": r.dup_count,
                    "language": r.language,
                }))
        })
        .collect())
}

#[derive(PartialEq, Clone, Copy)]
enum Case {
    Snake,
    Camel,
    Other,
}

fn classify_case(name: &str) -> Case {
    let core = name.trim_start_matches('_');
    if core.is_empty() || core.chars().next().is_some_and(|c| c.is_uppercase()) {
        return Case::Other; // PascalCase / empty — not a function-name style we judge
    }
    let has_underscore = core.contains('_');
    let has_upper = core.chars().any(|c| c.is_uppercase());
    match (has_underscore, has_upper) {
        (true, false) => Case::Snake,
        (false, true) => Case::Camel,
        (false, false) => Case::Snake, // single lowercase word reads as snake
        (true, true) => Case::Other,   // mixed — don't judge
    }
}
