//! Cron job: Symbol extraction (Tier-0e tree-sitter pass).
//!
//! Walks the indexed corpus and runs each registered `LanguageBackend` over
//! files whose `indexed_files.language` matches a backend. The per-file output
//! (symbols + references) is persisted into `file_symbols` and
//! `symbol_references`, the schema in `src/db/migrations.rs:417-470`.
//!
//! Mirrors `src/cron/graph_analysis.rs`'s shape end-to-end:
//! - Two-phase content fetch (Phase A metadata-only; Phase B content in 256-file batches).
//! - Per-project loop with per-project errors logged but not fatal.
//! - Per-file transaction wraps DELETE + INSERT to bound blast radius.
//!
//! Per-project watermark in `pgmcp_metadata['symbol_extraction_last_run:<id>']`
//! makes steady-state runs incremental — only files modified since the last
//! run are re-extracted.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use chrono::Utc;
use sqlx::PgPool;
use tracing::{error, info, warn};

use crate::db::DbClient;
use crate::db::queries;
use crate::parsing::{LanguageRegistry, symbols::Symbol, symbols::SymbolKind};
use crate::stats::tracker::StatsTracker;

/// Size of each content-fetch batch. Mirrors `graph_analysis.rs:41`.
const CONTENT_BATCH_SIZE: usize = 256;

/// Languages that have a registered `LanguageBackend`. Kept in sync with
/// `LanguageRegistry::for_language` in `src/parsing/mod.rs:73-92`. Used as
/// the SQL filter for Phase A so we never fetch content for files we'd skip.
const BACKEND_LANGUAGES: &[&str] = &[
    "rust",
    "python",
    "javascript",
    "typescript",
    "tsx",
    "java",
    "scala",
    "c",
    "cpp",
    "rholang",
    "clojure",
    "clojurescript",
];

/// Run the full symbol-extraction pipeline across all projects.
pub async fn run_symbol_extraction(db: &dyn DbClient, stats: &Arc<StatsTracker>) {
    let pool = db.pool().expect(
        "symbol_extraction requires a real &PgPool — DbClient backend must be PgPool-backed",
    );

    info!("Starting symbol-extraction cron job");
    let start = std::time::Instant::now();

    let projects: Vec<(i32, String)> =
        match sqlx::query_as::<_, (i32, String)>("SELECT id, name FROM projects ORDER BY id")
            .fetch_all(pool)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to list projects for symbol extraction: {}", e);
                return;
            }
        };

    let mut total_files: u64 = 0;
    let mut total_symbols: u64 = 0;
    let mut total_refs: u64 = 0;

    for (project_id, project_name) in &projects {
        match extract_project_symbols(pool, *project_id, project_name, stats).await {
            Ok(stats_per_project) => {
                total_files += stats_per_project.files_processed;
                total_symbols += stats_per_project.symbols_inserted;
                total_refs += stats_per_project.refs_inserted;
            }
            Err(e) => {
                error!(
                    project = %project_name,
                    error = %e,
                    "Symbol extraction failed for project"
                );
            }
        }
    }

    stats.symbol_extraction_runs.fetch_add(1, Ordering::Relaxed);
    info!(
        elapsed_ms = start.elapsed().as_millis() as u64,
        projects = projects.len(),
        files = total_files,
        symbols = total_symbols,
        references = total_refs,
        "Symbol-extraction cron job complete"
    );
}

/// Per-project cumulative counters returned by `extract_project_symbols`.
#[derive(Default)]
struct ProjectExtractionStats {
    files_processed: u64,
    symbols_inserted: u64,
    refs_inserted: u64,
}

/// Extract symbols + references for one project.
///
/// Two-phase content fetch (mirrors `graph_analysis.rs::analyze_project`):
///   Phase A: metadata only — id, path, language, modified_at — small.
///   Phase B: content in 256-file batches — extract via backend, persist, drop.
async fn extract_project_symbols(
    pool: &PgPool,
    project_id: i32,
    project_name: &str,
    stats: &Arc<StatsTracker>,
) -> Result<ProjectExtractionStats, sqlx::Error> {
    let watermark = queries::get_symbol_extraction_watermark(pool, project_id).await?;
    let phase_a_start = std::time::Instant::now();
    let metas =
        queries::list_files_for_symbol_extraction(pool, project_id, BACKEND_LANGUAGES, watermark)
            .await?;

    if metas.is_empty() {
        info!(
            project = %project_name,
            watermark = ?watermark,
            "Symbol extraction: no files to process"
        );
        // Still bump the watermark so subsequent no-op runs are cheap.
        queries::set_symbol_extraction_watermark(pool, project_id, Utc::now()).await?;
        return Ok(ProjectExtractionStats::default());
    }

    info!(
        project = %project_name,
        files = metas.len(),
        watermark = ?watermark,
        phase_a_ms = phase_a_start.elapsed().as_millis() as u64,
        "Symbol extraction Phase A complete"
    );

    let file_ids: Vec<i64> = metas.iter().map(|m| m.file_id).collect();
    let mut counters = ProjectExtractionStats::default();

    for batch_ids in file_ids.chunks(CONTENT_BATCH_SIZE) {
        let batch = queries::fetch_file_content_batch(pool, project_id, batch_ids).await?;

        for file in &batch {
            let content = match &file.content {
                Some(c) => c,
                None => continue,
            };

            match extract_and_persist_file(pool, file.file_id, &file.language, content, stats).await
            {
                Ok((s, r)) => {
                    counters.files_processed += 1;
                    counters.symbols_inserted += s;
                    counters.refs_inserted += r;
                }
                Err(e) => {
                    // Per-file transaction failures are logged and skipped — the
                    // FK CASCADE handles the case where the file was deleted
                    // between Phase A and Phase B.
                    warn!(
                        project = %project_name,
                        file = %file.relative_path,
                        error = %e,
                        "Symbol extraction failed for file (skipping)"
                    );
                }
            }
        }
        // batch dropped → content strings freed before next fetch
    }

    // Per-project second pass — resolve target_symbol_id by name match.
    let resolve_start = std::time::Instant::now();
    let resolved = queries::resolve_symbol_reference_targets(pool, project_id).await?;
    info!(
        project = %project_name,
        resolved_targets = resolved,
        resolve_ms = resolve_start.elapsed().as_millis() as u64,
        "Symbol-reference target resolution complete"
    );

    queries::set_symbol_extraction_watermark(pool, project_id, Utc::now()).await?;
    info!(
        project = %project_name,
        files = counters.files_processed,
        symbols = counters.symbols_inserted,
        references = counters.refs_inserted,
        "Symbol extraction complete for project"
    );

    Ok(counters)
}

/// Extract + persist for a single file. Wrapped in one transaction so the
/// DELETE + INSERT pair is atomic; rollback on FK violation (file deleted
/// concurrently) is the cron's FK-drift mitigation.
///
/// Returns `(symbols_inserted, references_inserted)` on success.
async fn extract_and_persist_file(
    pool: &PgPool,
    file_id: i64,
    language: &str,
    content: &str,
    stats: &Arc<StatsTracker>,
) -> Result<(u64, u64), sqlx::Error> {
    let backend = match LanguageRegistry::for_language(language) {
        Some(b) => b,
        None => return Ok((0, 0)),
    };

    // Run the backends outside the transaction (CPU work).
    let mut symbols = backend.extract_symbols(content);
    let mut references = backend.extract_references(content);

    if symbols.is_empty() && references.is_empty() {
        // Nothing to persist; still scrub stale rows for this file.
        let mut tx = pool.begin().await?;
        sqlx::query("DELETE FROM file_symbols WHERE file_id = $1")
            .bind(file_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM symbol_references WHERE source_file_id = $1")
            .bind(file_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok((0, 0));
    }

    // Dedupe symbols by UNIQUE (file_id, kind, name, start_line). Backends
    // shouldn't produce duplicates, but defensive dedupe keeps the bulk
    // INSERT's UNNEST ON CONFLICT path simple.
    {
        let mut seen: HashSet<(SymbolKind, String, u32)> = HashSet::with_capacity(symbols.len());
        symbols.retain(|s| seen.insert((s.kind, s.name.clone(), s.start_line)));
    }
    // Dedupe references by UNIQUE (source_file_id, source_line, target_raw, ref_kind).
    {
        let mut seen: HashSet<(u32, String, String)> = HashSet::with_capacity(references.len());
        references.retain(|r| {
            seen.insert((
                r.source_line,
                r.target_raw.clone(),
                r.ref_kind.as_db_str().to_string(),
            ))
        });
    }

    // Per-file transaction.
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM file_symbols WHERE file_id = $1")
        .bind(file_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM symbol_references WHERE source_file_id = $1")
        .bind(file_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    // Bulk-insert symbols (returns ids in input order so we can resolve parent_id).
    let symbol_ids = queries::bulk_insert_file_symbols(pool, file_id, &symbols).await?;
    debug_assert_eq!(symbol_ids.len(), symbols.len());

    // In-Rust parent_id resolution — for each Function whose start_line falls
    // inside a Struct/Class/Trait/Interface's [start_line, end_line], set
    // parent_id to that container.
    let parent_pairs = compute_parent_pairs(&symbols, &symbol_ids);
    if !parent_pairs.is_empty() {
        queries::update_symbol_parent_ids(pool, &parent_pairs).await?;
    }

    // In-Rust source_symbol_id resolution — for each reference at line L,
    // pick the smallest-range symbol whose [start_line, end_line] contains L.
    resolve_source_symbol_ids(&symbols, &symbol_ids, &mut references);

    // Bulk-insert references.
    let refs_inserted = queries::bulk_insert_symbol_references(pool, file_id, &references).await?;

    // Bump per-file stats.
    stats
        .symbols_extracted
        .fetch_add(symbols.len() as u64, Ordering::Relaxed);
    stats
        .symbol_references_inserted
        .fetch_add(refs_inserted, Ordering::Relaxed);

    Ok((symbols.len() as u64, refs_inserted))
}

/// Compute `(child_symbol_id, parent_symbol_id)` pairs for the in-file
/// container relationship: a Function inside a Struct/Class/Trait/Interface
/// at the matching line range gets that container as parent.
fn compute_parent_pairs(symbols: &[Symbol], ids: &[i64]) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    let containers: Vec<(usize, &Symbol)> = symbols
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            matches!(
                s.kind,
                SymbolKind::Struct
                    | SymbolKind::Class
                    | SymbolKind::Trait
                    | SymbolKind::Interface
                    | SymbolKind::Enum
            )
        })
        .collect();

    for (i, sym) in symbols.iter().enumerate() {
        if !matches!(sym.kind, SymbolKind::Function) {
            continue;
        }
        // Find the smallest-range container whose [start, end] contains
        // sym.start_line. "Smallest-range" matters for nested impls.
        let mut best: Option<(usize, u32)> = None;
        for (ci, c) in &containers {
            if c.start_line <= sym.start_line && sym.start_line <= c.end_line {
                let span = c.end_line.saturating_sub(c.start_line);
                if best.map(|(_, b)| span < b).unwrap_or(true) {
                    best = Some((*ci, span));
                }
            }
        }
        if let Some((ci, _)) = best
            && let (Some(child_id), Some(parent_id)) = (ids.get(i), ids.get(ci))
            && *child_id != 0
            && *parent_id != 0
        {
            out.push((*child_id, *parent_id));
        }
    }
    out
}

/// For each reference, set `source_symbol_id` to the smallest-range symbol
/// whose `[start_line, end_line]` contains `reference.source_line`. Mutates
/// `references` in place.
fn resolve_source_symbol_ids(
    symbols: &[Symbol],
    ids: &[i64],
    references: &mut [crate::parsing::symbols::SymbolReference],
) {
    for r in references.iter_mut() {
        if r.source_symbol_id.is_some() {
            continue;
        }
        let mut best: Option<(i64, u32)> = None;
        for (i, s) in symbols.iter().enumerate() {
            if s.start_line <= r.source_line && r.source_line <= s.end_line {
                let span = s.end_line.saturating_sub(s.start_line);
                if best.map(|(_, b)| span < b).unwrap_or(true)
                    && let Some(id) = ids.get(i)
                    && *id != 0
                {
                    best = Some((*id, span));
                }
            }
        }
        if let Some((id, _)) = best {
            r.source_symbol_id = Some(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsing::symbols::{Symbol, SymbolKind, SymbolRefKind, SymbolReference};

    #[test]
    fn parent_resolution_picks_innermost_container() {
        let symbols = vec![
            Symbol {
                file_id: 0,
                name: "OuterStruct".into(),
                kind: SymbolKind::Struct,
                start_line: 1,
                end_line: 100,
                parent_id: None,
                visibility: None,
                signature: None,
            },
            Symbol {
                file_id: 0,
                name: "InnerStruct".into(),
                kind: SymbolKind::Struct,
                start_line: 10,
                end_line: 50,
                parent_id: None,
                visibility: None,
                signature: None,
            },
            Symbol {
                file_id: 0,
                name: "method_a".into(),
                kind: SymbolKind::Function,
                start_line: 20,
                end_line: 25,
                parent_id: None,
                visibility: None,
                signature: None,
            },
        ];
        let ids = vec![10i64, 11, 12];
        let pairs = compute_parent_pairs(&symbols, &ids);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], (12, 11)); // method_a → InnerStruct (smallest container)
    }

    #[test]
    fn parent_resolution_empty_when_no_containers() {
        let symbols = vec![Symbol {
            file_id: 0,
            name: "free_fn".into(),
            kind: SymbolKind::Function,
            start_line: 1,
            end_line: 5,
            parent_id: None,
            visibility: None,
            signature: None,
        }];
        let ids = vec![1];
        let pairs = compute_parent_pairs(&symbols, &ids);
        assert!(pairs.is_empty());
    }

    #[test]
    fn source_symbol_resolution_picks_innermost() {
        let symbols = vec![
            Symbol {
                file_id: 0,
                name: "OuterFn".into(),
                kind: SymbolKind::Function,
                start_line: 1,
                end_line: 100,
                parent_id: None,
                visibility: None,
                signature: None,
            },
            Symbol {
                file_id: 0,
                name: "InnerClosure".into(),
                kind: SymbolKind::Function,
                start_line: 30,
                end_line: 40,
                parent_id: None,
                visibility: None,
                signature: None,
            },
        ];
        let ids = vec![10i64, 11];
        let mut refs = vec![SymbolReference {
            source_file_id: 0,
            source_symbol_id: None,
            target_file_id: None,
            target_symbol_id: None,
            target_raw: "do_thing".into(),
            ref_kind: SymbolRefKind::Call,
            source_line: 35,
        }];
        resolve_source_symbol_ids(&symbols, &ids, &mut refs);
        assert_eq!(refs[0].source_symbol_id, Some(11));
    }
}
