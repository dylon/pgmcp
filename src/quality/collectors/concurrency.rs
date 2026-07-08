//! Concurrency & safety collectors. `panic_paths`/`unsafe_clusters` read the
//! typed `function_metrics`; the rest are Rust-flavored content-regex scans.

use regex::Regex;
use rmcp::ErrorData as McpError;
use serde_json::json;

use super::truncate_preview;
use crate::context::SystemContext;
use crate::mcp::tools::sota_helpers::pool_or_err;
use crate::quality::findings::{Finding, FindingCategory, Severity};

const CC: FindingCategory = FindingCategory::Concurrency;

/// `(path, content)` for every text file in the project. Delegates to the shared
/// [`super::load_project_file_contents`] loader (single SQL + PG-timeout lift),
/// dropping rows whose (nullable) content is absent so callers get a plain
/// `String`.
async fn project_contents(
    ctx: &SystemContext,
    project_id: i32,
) -> Result<Vec<(String, String)>, McpError> {
    let pool = pool_or_err(ctx)?;
    Ok(super::load_project_file_contents(pool, project_id, None)
        .await?
        .into_iter()
        .filter_map(|(path, content)| content.map(|c| (path, c)))
        .collect())
}

/// Blocking calls inside files that also use `async fn`.
pub async fn collect_blocking_in_async(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let contents = project_contents(ctx, project_id).await?;
    let blocking =
        Regex::new(r"(std::thread::sleep|std::fs::(read|write|File)|reqwest::blocking|\.lock\(\)\.unwrap\(\)|block_on)")
            .expect("re");
    let mut out = Vec::new();
    for (path, content) in &contents {
        if !content.contains("async fn") {
            continue;
        }
        for (i, line) in content.lines().enumerate() {
            if let Some(m) = blocking.find(line) {
                out.push(
                    Finding::new(
                        "blocking_in_async",
                        CC,
                        project_name,
                        Severity::Medium,
                        format!("Blocking call `{}` in an async file", m.as_str()),
                    )
                    .at(path, (i + 1) as u32)
                    .with_kind("blocking_in_async")
                    .with_raw(json!({ "file": path, "line": i + 1, "blocking_call": m.as_str() })),
                );
            }
        }
    }
    Ok(out)
}

/// `static mut` — a data-race hazard.
pub async fn collect_lockset_races(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let contents = project_contents(ctx, project_id).await?;
    let re = Regex::new(r"\bstatic\s+mut\b").expect("re");
    let mut out = Vec::new();
    for (path, content) in &contents {
        for (i, line) in content.lines().enumerate() {
            if re.is_match(line) {
                out.push(
                    Finding::new(
                        "lockset_races",
                        CC,
                        project_name,
                        Severity::High,
                        format!(
                            "`static mut` shared mutable state: {}",
                            truncate_preview(line, 80)
                        ),
                    )
                    .at(path, (i + 1) as u32)
                    .with_kind("static_mut")
                    .with_raw(json!({ "file": path, "line": i + 1 })),
                );
            }
        }
    }
    Ok(out)
}

/// `unsafe impl Send`/`Sync` — hand-asserted thread-safety to audit.
pub async fn collect_send_sync_violations(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let contents = project_contents(ctx, project_id).await?;
    let re = Regex::new(r"unsafe\s+impl\s+(Send|Sync)\b").expect("re");
    let mut out = Vec::new();
    for (path, content) in &contents {
        for (i, line) in content.lines().enumerate() {
            if re.is_match(line) {
                out.push(
                    Finding::new(
                        "send_sync_violations",
                        CC,
                        project_name,
                        Severity::Medium,
                        format!(
                            "Hand-asserted thread-safety: {}",
                            truncate_preview(line, 80)
                        ),
                    )
                    .at(path, (i + 1) as u32)
                    .with_kind("unsafe_send_sync")
                    .with_raw(json!({ "file": path, "line": i + 1 })),
                );
            }
        }
    }
    Ok(out)
}

/// Repeated lock acquisition in `Arc<Mutex>`-using files (deadlock review).
pub async fn collect_deadlock_candidates(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<Vec<Finding>, McpError> {
    let contents = project_contents(ctx, project_id).await?;
    let lock = Regex::new(r"\.lock\(\)").expect("re");
    let mut out = Vec::new();
    for (path, content) in &contents {
        let locks = lock.find_iter(content).count();
        if locks >= 2 && (content.contains("Arc<Mutex") || content.contains("Arc<RwLock")) {
            out.push(
                Finding::new(
                    "deadlock_candidates",
                    CC,
                    project_name,
                    Severity::Low,
                    format!(
                        "{path} acquires {locks} locks over shared state — review lock ordering"
                    ),
                )
                .with_score(locks as f64)
                .at_file(path)
                .with_kind("multi_lock")
                .with_raw(json!({ "file": path, "lock_sites": locks })),
            );
        }
    }
    Ok(out)
}

/// Functions with panic-reachable paths (typed `function_metrics`).
pub async fn collect_panic_paths(
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
        panic_paths: i32,
        cyclomatic: i32,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT f.relative_path, fs.name, fs.start_line, fm.panic_paths, fm.cyclomatic
         FROM function_metrics fm
         JOIN file_symbols fs ON fs.id = fm.function_id
         JOIN indexed_files f ON f.id = fm.file_id
         WHERE fm.project_id = $1 AND fm.panic_paths > 0
         ORDER BY fm.panic_paths DESC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("panic_paths query failed: {e}"), None))?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let severity = match r.panic_paths {
                p if p >= 3 => Severity::High,
                _ => Severity::Medium,
            };
            Finding::new(
                "panic_paths",
                CC,
                project_name,
                severity,
                format!("{}() has {} panic-reachable path(s) (cyclomatic {})", r.name, r.panic_paths, r.cyclomatic),
            )
            .with_score(r.panic_paths as f64)
            .at(&r.relative_path, r.start_line.max(0) as u32)
            .with_kind("panic_path")
            .with_raw(json!({ "file": r.relative_path, "function": r.name, "panic_paths": r.panic_paths }))
        })
        .collect())
}

/// Functions containing `unsafe` blocks (typed `function_metrics`).
pub async fn collect_unsafe_clusters(
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
        unsafe_blocks: i32,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT f.relative_path, fs.name, fs.start_line, fm.unsafe_blocks
         FROM function_metrics fm
         JOIN file_symbols fs ON fs.id = fm.function_id
         JOIN indexed_files f ON f.id = fm.file_id
         WHERE fm.project_id = $1
           AND f.project_id = fm.project_id
           AND fs.file_id = f.id
           AND fm.unsafe_blocks > 0
         ORDER BY fm.unsafe_blocks DESC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("unsafe_clusters query failed: {e}"), None))?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let severity = if r.unsafe_blocks >= 3 { Severity::Medium } else { Severity::Low };
            Finding::new(
                "unsafe_clusters",
                CC,
                project_name,
                severity,
                format!("{}() contains {} unsafe block(s)", r.name, r.unsafe_blocks),
            )
            .with_score(r.unsafe_blocks as f64)
            .at(&r.relative_path, r.start_line.max(0) as u32)
            .with_kind("unsafe_block")
            .with_raw(json!({ "file": r.relative_path, "function": r.name, "unsafe_blocks": r.unsafe_blocks }))
        })
        .collect())
}
