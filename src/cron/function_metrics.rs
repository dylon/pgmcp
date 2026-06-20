//! Cron job: per-function complexity metrics (SOTA Phase 1, A1).
//!
//! Walks the corpus and runs `LanguageBackend::extract_function_metrics` over
//! files whose language has a backend. For each extracted `FunctionMetrics`
//! row, resolves the `function_id` via `file_symbols` lookup on
//! `(file_id, kind='function', name, start_line)`, then bulk-upserts into
//! `function_metrics`.
//!
//! Sequenced after `symbol-extraction` (which produces the `file_symbols`
//! rows this cron depends on for `function_id` resolution). Shares the
//! `heavy_cron_lock` with the rest of the heavy quartet.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use chrono::Utc;
use sqlx::PgPool;
use tracing::{error, info, warn};

use crate::db::DbClient;
use crate::db::queries::{self, FunctionMetricsRow};
use crate::parsing::LanguageRegistry;
use crate::parsing::function_metrics::FunctionMetrics;
use crate::stats::tracker::StatsTracker;

/// Size of each content-fetch batch. Mirrors symbol_extraction's batching.
const CONTENT_BATCH_SIZE: usize = 256;

/// Languages whose backend has a non-default `extract_function_metrics`. As
/// new backends grow the impl, append to this list.
const FUNCTION_METRICS_LANGUAGES: &[&str] = &[
    "rust",
    "python",
    "rholang",
    "typescript",
    "tsx",
    "javascript",
    "clojure",
    "clojurescript",
];

/// Run the full function-metrics pipeline across all projects.
pub async fn run_function_metrics(db: &dyn DbClient, stats: &Arc<StatsTracker>) {
    let pool = db.pool().expect(
        "function_metrics requires a real &PgPool — DbClient backend must be PgPool-backed",
    );
    info!("Starting function-metrics cron job");
    let start = std::time::Instant::now();

    // Promoted to top-of-body: pairs with `function_metrics_noop_returns`
    // to distinguish "ran, no projects" from "never ran".
    stats.function_metrics_runs.fetch_add(1, Ordering::Relaxed);

    let projects: Vec<(i32, String)> =
        match sqlx::query_as::<_, (i32, String)>("SELECT id, name FROM projects ORDER BY id")
            .fetch_all(pool)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to list projects for function-metrics: {}", e);
                return;
            }
        };

    if projects.is_empty() {
        stats
            .function_metrics_noop_returns
            .fetch_add(1, Ordering::Relaxed);
        info!("Function-metrics cron: no projects to score");
        return;
    }

    run_function_metrics_over(pool, stats, &projects, start).await;
}

/// Run function-metrics scoring for a SINGLE project (by name or numeric id).
/// Operator-facing per-project trigger (F2).
pub async fn run_function_metrics_for_project(
    db: &dyn DbClient,
    stats: &Arc<StatsTracker>,
    project_ref: &str,
) {
    let pool = db.pool().expect(
        "function_metrics requires a real &PgPool — DbClient backend must be PgPool-backed",
    );
    info!(project = %project_ref, "Starting single-project function-metrics");
    let start = std::time::Instant::now();
    stats.function_metrics_runs.fetch_add(1, Ordering::Relaxed);

    let projects: Vec<(i32, String)> = match sqlx::query_as::<_, (i32, String)>(
        "SELECT id, name FROM projects WHERE name = $1 OR id::text = $1 ORDER BY id",
    )
    .bind(project_ref)
    .fetch_all(pool)
    .await
    {
        Ok(p) => p,
        Err(e) => {
            error!(project = %project_ref, error = %e, "Failed to resolve project for function-metrics");
            return;
        }
    };

    if projects.is_empty() {
        stats
            .function_metrics_noop_returns
            .fetch_add(1, Ordering::Relaxed);
        warn!(project = %project_ref, "Function-metrics: no project matched name or id");
        return;
    }

    run_function_metrics_over(pool, stats, &projects, start).await;
}

/// Shared driver: score a resolved project set and emit the completion log.
async fn run_function_metrics_over(
    pool: &PgPool,
    stats: &Arc<StatsTracker>,
    projects: &[(i32, String)],
    start: std::time::Instant,
) {
    let mut total_files: u64 = 0;
    let mut total_functions: u64 = 0;

    for (project_id, project_name) in projects {
        match score_project_functions(pool, *project_id, project_name, stats).await {
            Ok(per_project) => {
                total_files += per_project.files_processed;
                total_functions += per_project.functions_scored;
            }
            Err(e) => {
                error!(
                    project = %project_name,
                    error = %e,
                    "Function-metrics failed for project"
                );
            }
        }
    }

    // `function_metrics_runs` was promoted to top-of-body above.
    info!(
        elapsed_ms = start.elapsed().as_millis() as u64,
        projects = projects.len(),
        files = total_files,
        functions = total_functions,
        "Function-metrics cron job complete"
    );
}

#[derive(Default)]
struct ProjectScoringStats {
    files_processed: u64,
    functions_scored: u64,
}

async fn score_project_functions(
    pool: &PgPool,
    project_id: i32,
    project_name: &str,
    stats: &Arc<StatsTracker>,
) -> Result<ProjectScoringStats, sqlx::Error> {
    let mut watermark = queries::get_function_metrics_watermark(pool, project_id).await?;
    // Self-heal the advance-on-empty watermark trap (mirrors symbol-extraction):
    // backend files present but `function_metrics` empty means a prior run
    // advanced the watermark without persisting rows; force a full re-scan so
    // per-function complexity backfills instead of staying dark forever.
    if watermark.is_some()
        && queries::project_missing_function_metrics(pool, project_id, FUNCTION_METRICS_LANGUAGES)
            .await?
    {
        info!(
            project = %project_name,
            "Function-metrics: backend files present but function_metrics empty; forcing full re-scan to backfill"
        );
        watermark = None;
    }
    let phase_a_start = std::time::Instant::now();
    let metas = queries::list_files_for_symbol_extraction(
        pool,
        project_id,
        FUNCTION_METRICS_LANGUAGES,
        watermark,
    )
    .await?;

    if metas.is_empty() {
        info!(
            project = %project_name,
            watermark = ?watermark,
            "Function-metrics: no files to process"
        );
        queries::set_function_metrics_watermark(pool, project_id, Utc::now()).await?;
        return Ok(ProjectScoringStats::default());
    }

    info!(
        project = %project_name,
        files = metas.len(),
        watermark = ?watermark,
        phase_a_ms = phase_a_start.elapsed().as_millis() as u64,
        "Function-metrics Phase A complete"
    );

    let file_ids: Vec<i64> = metas.iter().map(|m| m.file_id).collect();
    let mut counters = ProjectScoringStats::default();

    for batch_ids in file_ids.chunks(CONTENT_BATCH_SIZE) {
        let batch = queries::fetch_file_content_batch(pool, project_id, batch_ids).await?;
        for file in &batch {
            let content = match &file.content {
                Some(c) => c,
                None => continue,
            };
            match score_one_file(pool, project_id, file.file_id, &file.language, content).await {
                Ok(scored) => {
                    counters.files_processed += 1;
                    counters.functions_scored += scored;
                    stats.functions_scored.fetch_add(scored, Ordering::Relaxed);
                }
                Err(e) => {
                    error!(
                        project = %project_name,
                        file = %file.relative_path,
                        error = %e,
                        "Function-metrics failed for file (skipping)"
                    );
                }
            }
        }
    }

    queries::set_function_metrics_watermark(pool, project_id, Utc::now()).await?;
    info!(
        project = %project_name,
        files = counters.files_processed,
        functions = counters.functions_scored,
        "Function-metrics complete for project"
    );
    Ok(counters)
}

async fn score_one_file(
    pool: &PgPool,
    project_id: i32,
    file_id: i64,
    language: &str,
    content: &str,
) -> Result<u64, sqlx::Error> {
    let backend = match LanguageRegistry::for_language(language) {
        Some(b) => b,
        None => return Ok(0),
    };
    // CPU work outside any transaction.
    let metrics: Vec<FunctionMetrics> = backend.extract_function_metrics(content);
    if metrics.is_empty() {
        return Ok(0);
    }

    // Resolve function_id via file_symbols(file_id, kind='function', name, start_line).
    let symbols = queries::lookup_function_symbol_ids(pool, file_id).await?;
    let mut by_key: HashMap<(String, i32), i64> = HashMap::with_capacity(symbols.len());
    for s in &symbols {
        by_key.insert((s.name.clone(), s.start_line), s.symbol_id);
    }

    let mut rows: Vec<FunctionMetricsRow> = Vec::with_capacity(metrics.len());
    for m in &metrics {
        let key = (m.name.clone(), m.start_line as i32);
        let Some(&function_id) = by_key.get(&key) else {
            // Symbol-extraction hasn't seen this function yet (maybe file
            // changed between symbol-extraction's run and this run, or the
            // language backend extracts more functions than the symbol
            // backend does). Skip — next pass will pick it up.
            continue;
        };
        let mi = m.maintainability_index();
        rows.push(FunctionMetricsRow {
            function_id,
            file_id,
            project_id,
            cyclomatic: m.cyclomatic as i32,
            cognitive: m.cognitive as i32,
            halstead_n1: m.halstead.n1 as i32,
            halstead_n2: m.halstead.n2 as i32,
            halstead_big_n1: m.halstead.big_n1 as i32,
            halstead_big_n2: m.halstead.big_n2 as i32,
            halstead_volume: m.halstead.volume(),
            halstead_difficulty: m.halstead.difficulty(),
            halstead_effort: m.halstead.effort(),
            halstead_bugs: m.halstead.bugs(),
            npath: m.npath.as_db_i64(),
            npath_overflow: m.npath.overflowed(),
            loc: m.loc as i32,
            comment_lines: m.comment_lines as i32,
            maintainability_index: mi,
            panic_paths: m.panic_paths as i32,
            unsafe_blocks: m.unsafe_blocks as i32,
        });
    }

    let inserted = queries::upsert_function_metrics_batch(pool, &rows).await?;
    Ok(inserted)
}
