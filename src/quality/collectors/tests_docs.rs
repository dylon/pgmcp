//! Tests & docs collectors: coverage gaps, test smells, flaky signals, doc
//! drift, mutation-kill surrogate.

use regex::Regex;
use rmcp::ErrorData as McpError;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::tools::sota_helpers::pool_or_err;
use crate::quality::findings::{Finding, FindingCategory, Severity};

const TD: FindingCategory = FindingCategory::TestsDocs;

/// Directories with substantial code but no test files.
pub async fn collect_test_coverage_gaps(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        dir: String,
        code: i64,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT dir, SUM(1 - is_test)::BIGINT AS code FROM (
            SELECT COALESCE((regexp_split_to_array(relative_path,'/'))[1],'.') AS dir,
                   CASE WHEN relative_path ~* '(test|spec|_test\\.|_spec\\.)' THEN 1 ELSE 0 END AS is_test
            FROM indexed_files
            WHERE project_id = $1 AND language NOT IN ('markdown','text','json','toml')
         ) t
         GROUP BY dir
         HAVING SUM(1 - is_test) >= 5 AND SUM(is_test) = 0",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("test_coverage_gaps query failed: {e}"), None))?;

    Ok(rows
        .into_iter()
        .map(|r| {
            Finding::new(
                "test_coverage_gaps",
                TD,
                project_name,
                Severity::Medium,
                format!("`{}` has {} code files but no tests", r.dir, r.code),
            )
            .with_score(r.code as f64)
            .with_kind(format!("untested_dir:{}", r.dir))
            .with_raw(json!({ "directory": r.dir, "code_files": r.code }))
        })
        .collect())
}

/// Directories with substantial code but no markdown docs.
pub async fn collect_doc_coverage_gaps(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        dir: String,
        code: i64,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT dir, SUM(1 - is_md)::BIGINT AS code FROM (
            SELECT COALESCE((regexp_split_to_array(relative_path,'/'))[1],'.') AS dir,
                   CASE WHEN language = 'markdown' THEN 1 ELSE 0 END AS is_md
            FROM indexed_files WHERE project_id = $1
         ) t
         GROUP BY dir
         HAVING SUM(1 - is_md) >= 8 AND SUM(is_md) = 0",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("doc_coverage_gaps query failed: {e}"), None))?;

    Ok(rows
        .into_iter()
        .map(|r| {
            Finding::new(
                "doc_coverage_gaps",
                TD,
                project_name,
                Severity::Low,
                format!("`{}` has {} files but no markdown docs", r.dir, r.code),
            )
            .with_score(r.code as f64)
            .with_kind(format!("undocumented_dir:{}", r.dir))
            .with_raw(json!({ "directory": r.dir, "files": r.code }))
        })
        .collect())
}

/// Oversized test files.
pub async fn collect_test_smells(
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
        "SELECT relative_path, line_count FROM indexed_files
         WHERE project_id = $1 AND relative_path ~* '(test|spec|_test\\.|_spec\\.)'
           AND line_count > 400
         ORDER BY line_count DESC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("test_smells query failed: {e}"), None))?;

    Ok(rows
        .into_iter()
        .map(|r| {
            Finding::new(
                "test_smells",
                TD,
                project_name,
                Severity::Low,
                format!(
                    "{} is a large test file ({} lines) — consider splitting",
                    r.relative_path, r.line_count
                ),
            )
            .with_score(r.line_count as f64)
            .at_file(&r.relative_path)
            .with_kind("large_test")
            .with_raw(json!({ "path": r.relative_path, "line_count": r.line_count }))
        })
        .collect())
}

/// Test files exhibiting flakiness signals (timing, randomness, network).
pub async fn collect_flaky_test_candidates(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let pool = pool_or_err(ctx)?;
    // Corpus-scale content scan — routed through the shared PG-timeout-lifted
    // loader so a large project's read is not cancelled at the pool's 30 s default.
    // The `(?i)` prefix preserves the original `~*` case-insensitive test-file
    // filter (the shared loader's operator is the case-sensitive `~`).
    let rows = super::load_project_file_contents(
        pool,
        project_id,
        Some(r"(?i)(test|spec|_test\.|_spec\.)"),
    )
    .await?;

    let re = Regex::new(
        r"(?i)(thread::sleep|time\.sleep|Instant::now|SystemTime::now|rand::|random\(|Math\.random|reqwest|TcpStream|\.connect\()",
    )
    .expect("re");
    let mut out = Vec::new();
    for (relative_path, content) in &rows {
        let content = content.as_deref().unwrap_or("");
        let mut signals: Vec<&str> = Vec::new();
        for cap in re.find_iter(content) {
            let s = cap.as_str();
            if !signals.iter().any(|x| x.eq_ignore_ascii_case(s)) {
                signals.push(s);
            }
        }
        if !signals.is_empty() {
            out.push(
                Finding::new(
                    "flaky_test_candidates",
                    TD,
                    project_name,
                    Severity::Low,
                    format!(
                        "{} shows flakiness signals: {}",
                        relative_path,
                        signals.join(", ")
                    ),
                )
                .at_file(relative_path)
                .with_kind("flaky_signals")
                .with_raw(json!({ "path": relative_path, "signals": signals })),
            );
        }
    }
    Ok(out)
}

/// Stale markdown — documentation untouched for a long time (drift risk).
pub async fn collect_doc_code_drift(
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
         WHERE f.project_id = $1 AND f.language = 'markdown'
           AND fm.days_since_last_change > 180
         ORDER BY fm.days_since_last_change DESC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("doc_code_drift query failed: {e}"), None))?;

    Ok(rows
        .into_iter()
        .map(|r| {
            Finding::new(
                "doc_code_drift",
                TD,
                project_name,
                Severity::Low,
                format!(
                    "{} untouched {} days — may have drifted from the code",
                    r.relative_path, r.days
                ),
            )
            .with_score(r.days as f64)
            .at_file(&r.relative_path)
            .with_kind("stale_doc")
            .with_raw(json!({ "path": r.relative_path, "days_idle": r.days }))
        })
        .collect())
}

/// High-complexity functions outside tests — mutation-kill is hardest here, so
/// they most need strong tests (mutation-score surrogate).
pub async fn collect_mutation_score_surrogate(
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
        cyclomatic: i32,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT f.relative_path, fs.name, fs.start_line, fm.cyclomatic
         FROM function_metrics fm
         JOIN file_symbols fs ON fs.id = fm.function_id
         JOIN indexed_files f ON f.id = fm.file_id
         WHERE fm.project_id = $1 AND fm.cyclomatic > 10
           AND f.relative_path !~* '(test|spec|_test\\.|_spec\\.)'
         ORDER BY fm.cyclomatic DESC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("mutation_score query failed: {e}"), None))?;

    Ok(rows
        .into_iter()
        .map(|r| {
            Finding::new(
                "mutation_score_surrogate",
                TD,
                project_name,
                Severity::Low,
                format!(
                    "{}() is complex (cyclomatic {}) — ensure mutation-strong tests",
                    r.name, r.cyclomatic
                ),
            )
            .with_score(r.cyclomatic as f64)
            .at(&r.relative_path, r.start_line.max(0) as u32)
            .with_kind("hard_to_test")
            .with_raw(
                json!({ "file": r.relative_path, "function": r.name, "cyclomatic": r.cyclomatic }),
            )
        })
        .collect())
}
