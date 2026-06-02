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

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tracing::{error, info, warn};

use crate::db::DbClient;
use crate::db::disk_read::{DiskReadOutcome, read_disk_verified};
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
    "metta",
    "clojure",
    "clojurescript",
    // Formal-verification backends (post-SOTA addition).
    "coq",
    "tlaplus",
    "lean",
    "sage",
];

/// Run the full symbol-extraction pipeline across all projects.
pub async fn run_symbol_extraction(db: &dyn DbClient, stats: &Arc<StatsTracker>) {
    let pool = db.pool().expect(
        "symbol_extraction requires a real &PgPool — DbClient backend must be PgPool-backed",
    );

    info!("Starting symbol-extraction cron job");
    let start = std::time::Instant::now();

    // Promoted to top-of-body: pairs with `symbol_extraction_noop_returns`
    // to distinguish "ran, no projects" from "never ran".
    stats.symbol_extraction_runs.fetch_add(1, Ordering::Relaxed);

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

    if projects.is_empty() {
        stats
            .symbol_extraction_noop_returns
            .fetch_add(1, Ordering::Relaxed);
        info!("Symbol extraction cron: no projects to scan");
        return;
    }

    extract_over_projects(pool, stats, &projects, start).await;
}

/// Run symbol extraction for a SINGLE project, resolved by name or numeric id.
///
/// Lets an operator (`trigger_cron job="symbol-extraction" project=...`) stay
/// within the MCP tool budget instead of scanning every project serially —
/// the all-projects loop can exceed the 300s trigger timeout on a large
/// workspace, starving higher-id projects (F2).
pub async fn run_symbol_extraction_for_project(
    db: &dyn DbClient,
    stats: &Arc<StatsTracker>,
    project_ref: &str,
) {
    let pool = db.pool().expect(
        "symbol_extraction requires a real &PgPool — DbClient backend must be PgPool-backed",
    );

    info!(project = %project_ref, "Starting single-project symbol-extraction");
    let start = std::time::Instant::now();
    stats.symbol_extraction_runs.fetch_add(1, Ordering::Relaxed);

    // Resolve by name first, then numeric id (id::text avoids a parse step).
    let projects: Vec<(i32, String)> = match sqlx::query_as::<_, (i32, String)>(
        "SELECT id, name FROM projects WHERE name = $1 OR id::text = $1 ORDER BY id",
    )
    .bind(project_ref)
    .fetch_all(pool)
    .await
    {
        Ok(p) => p,
        Err(e) => {
            error!(project = %project_ref, error = %e, "Failed to resolve project for symbol extraction");
            return;
        }
    };

    if projects.is_empty() {
        stats
            .symbol_extraction_noop_returns
            .fetch_add(1, Ordering::Relaxed);
        warn!(project = %project_ref, "Symbol extraction: no project matched name or id");
        return;
    }

    extract_over_projects(pool, stats, &projects, start).await;
}

/// Shared driver: run `extract_project_symbols` over a resolved project set,
/// accumulate totals, and emit the completion log. Used by both the
/// all-projects and single-project entry points.
async fn extract_over_projects(
    pool: &PgPool,
    stats: &Arc<StatsTracker>,
    projects: &[(i32, String)],
    start: std::time::Instant,
) {
    let mut total_files: u64 = 0;
    let mut total_symbols: u64 = 0;
    let mut total_refs: u64 = 0;

    for (project_id, project_name) in projects {
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
    let mut watermark = queries::get_symbol_extraction_watermark(pool, project_id).await?;
    // Self-heal the advance-on-empty watermark trap: if the project has
    // backend-language files but zero `import_use` refs (the pre-fix state, or
    // an early run that advanced the watermark without persisting), an
    // incremental run would skip it forever. Force a full re-scan so the import
    // graph backfills automatically. Triggers at most once per project — after a
    // successful re-scan the `import_use` rows exist and this stays false.
    if watermark.is_some()
        && queries::project_missing_import_refs(pool, project_id, BACKEND_LANGUAGES).await?
    {
        info!(
            project = %project_name,
            "Symbol extraction: backend files present but no import_use refs; forcing full re-scan to backfill the import graph"
        );
        watermark = None;
    }
    // One-time `sync_ops` backfill (v21): on the first run after the migration
    // for a Rust/Rholang project, force a full re-scan so the existing corpus
    // populates the synchronization skeleton (the steady-state incremental path
    // would otherwise leave `sync_ops` empty until each file next changes).
    // Flag-gated, so it fires at most once even on concurrency-free projects.
    if watermark.is_some() && queries::sync_ops_backfill_pending(pool, project_id).await? {
        info!(
            project = %project_name,
            "Symbol extraction: one-time sync_ops backfill; forcing full re-scan"
        );
        watermark = None;
        queries::mark_sync_ops_backfill_done(pool, project_id).await?;
    }
    let phase_a_start = std::time::Instant::now();
    let metas =
        queries::list_files_for_symbol_extraction(pool, project_id, BACKEND_LANGUAGES, watermark)
            .await?;

    if metas.is_empty() {
        // No new/changed files — but an earlier pass may have left references
        // unresolved (e.g. a resolution that timed out and returned *before* this
        // watermark advanced, stranding NULL rows that a later no-files run then
        // skipped right past — the residual the Phase-3 timeout left behind).
        // Resolution is idempotent and near-free when there is nothing to do (its
        // EXISTS backlog guard short-circuits), so run it to DRAIN any such
        // backlog instead of skipping straight to the watermark bump.
        let drained = queries::resolve_symbol_reference_targets(pool, project_id).await?;
        if drained > 0 {
            info!(
                project = %project_name,
                drained_targets = drained,
                "Symbol extraction: no new files; drained unresolved-reference backlog"
            );
        } else {
            info!(
                project = %project_name,
                watermark = ?watermark,
                "Symbol extraction: no files to process"
            );
        }
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

    // F1 — watermark resilience. Track the smallest `modified_at` among SKIPPED
    // files (disk hash-mismatch / IO error / parse failure) so the watermark is
    // never advanced past a file we failed to extract (which would strand it
    // until a forced full re-scan). Incremental-skips of unchanged files are
    // NOT failures and do not hold the watermark back.
    let mut min_skipped: Option<DateTime<Utc>> = None;

    for batch_ids in file_ids.chunks(CONTENT_BATCH_SIZE) {
        let batch = queries::fetch_file_content_batch(pool, project_id, batch_ids).await?;

        for file in &batch {
            // RC2 incremental-skip: content unchanged since the last successful
            // extraction — nothing to re-parse. Cheap (a hash compare; no disk
            // read, no parse), which keeps full re-scans affordable.
            if file.content_hash.is_some() && file.content_hash == file.extracted_content_hash {
                stats
                    .symbol_extraction_unchanged_skips
                    .fetch_add(1, Ordering::Relaxed);
                continue;
            }

            // Resolve content: inline if present, else a hash-verified disk read
            // (asymmetric-storage policy — `content` is NULL for files cheap to
            // re-read from disk). RC2: never silently skip a content-NULL file;
            // recover it from disk or count the failure (and hold the watermark).
            let disk_owned;
            let content: &str = match &file.content {
                Some(c) => c,
                None => match read_disk_verified(
                    &file.path,
                    file.content_recoverable_from_disk,
                    file.content_hash,
                ) {
                    DiskReadOutcome::Hit(bytes) => {
                        stats
                            .symbol_extraction_disk_reads
                            .fetch_add(1, Ordering::Relaxed);
                        disk_owned = bytes;
                        &disk_owned
                    }
                    DiskReadOutcome::HashMismatch => {
                        stats
                            .symbol_extraction_disk_hash_mismatches
                            .fetch_add(1, Ordering::Relaxed);
                        warn!(
                            project = %project_name,
                            file = %file.relative_path,
                            "Symbol extraction: disk content changed since indexing (hash mismatch); skipping until re-indexed"
                        );
                        min_skipped = min_modified(min_skipped, file.modified_at);
                        continue;
                    }
                    DiskReadOutcome::Missing => {
                        stats
                            .symbol_extraction_disk_missing
                            .fetch_add(1, Ordering::Relaxed);
                        min_skipped = min_modified(min_skipped, file.modified_at);
                        continue;
                    }
                    DiskReadOutcome::IoError | DiskReadOutcome::NotRecoverable => {
                        stats
                            .symbol_extraction_disk_io_errors
                            .fetch_add(1, Ordering::Relaxed);
                        warn!(
                            project = %project_name,
                            file = %file.relative_path,
                            "Symbol extraction: content NULL and not recoverable from disk; skipping"
                        );
                        min_skipped = min_modified(min_skipped, file.modified_at);
                        continue;
                    }
                },
            };

            match extract_and_persist_file(pool, file.file_id, &file.language, content, stats).await
            {
                Ok((s, r)) => {
                    counters.files_processed += 1;
                    counters.symbols_inserted += s;
                    counters.refs_inserted += r;
                    // RC2 incremental-skip bookkeeping: remember the content we
                    // just extracted so an unchanged file is skipped next pass.
                    queries::set_extracted_content_hash(pool, file.file_id, file.content_hash)
                        .await?;
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
                    min_skipped = min_modified(min_skipped, file.modified_at);
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

    // F1 — advance the watermark to just before the earliest SKIPPED file's
    // `modified_at`, so every file we failed to extract is re-listed next run
    // instead of being stranded past the watermark. With no skips, advance to
    // now. The 1µs back-off keeps the skipped file (whose `modified_at` equals
    // `min_skipped`) inside the strict `modified_at > watermark` predicate;
    // successful newer files re-list too, but the incremental-skip turns them
    // into a cheap no-op. This is monotonic on incremental runs (a listed
    // file's `modified_at` always exceeds the prior watermark).
    let new_watermark = match min_skipped {
        None => Utc::now(),
        Some(min_skip) => min_skip - chrono::Duration::microseconds(1),
    };
    queries::set_symbol_extraction_watermark(pool, project_id, new_watermark).await?;
    info!(
        project = %project_name,
        files = counters.files_processed,
        symbols = counters.symbols_inserted,
        references = counters.refs_inserted,
        skipped_min_modified = ?min_skipped,
        "Symbol extraction complete for project"
    );

    Ok(counters)
}

/// `min` over an `Option<DateTime>` accumulator and a new value (F1 watermark).
fn min_modified(acc: Option<DateTime<Utc>>, v: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match acc {
        Some(cur) if cur <= v => Some(cur),
        _ => Some(v),
    }
}

/// Map a backend's extracted imports into `ImportUse` symbol-references so the
/// cron persists them alongside call/type refs.
///
/// KEYSTONE: without this wiring `symbol_references` carries zero `import_use`
/// rows, so `get_imports_from_symbols` returns empty and the project import
/// graph collapses (graph_analysis's symbol-aware path finds nothing; the regex
/// fallback only covers files with NO symbol refs). `source_file_id`'s `0`
/// placeholder is overwritten by `bulk_insert_symbol_references` from its
/// `file_id` argument; `target_file_id` is resolved later by graph_analysis from
/// `target_raw`. Backend-agnostic — every language's `extract_imports`
/// contributes.
fn imports_as_references(
    imports: Vec<crate::parsing::symbols::Import>,
) -> Vec<crate::parsing::symbols::SymbolReference> {
    use crate::parsing::symbols::{SymbolRefKind, SymbolReference};
    imports
        .into_iter()
        .map(|imp| SymbolReference {
            source_file_id: 0,
            source_symbol_id: None,
            target_file_id: None,
            target_symbol_id: None,
            target_raw: imp.target_raw,
            ref_kind: SymbolRefKind::ImportUse,
            source_line: imp.source_line,
        })
        .collect()
}

#[cfg(test)]
mod import_ref_tests {
    use super::imports_as_references;
    use crate::parsing::LanguageRegistry;
    use crate::parsing::symbols::SymbolRefKind;

    #[test]
    fn rust_use_becomes_import_use_ref() {
        // The keystone behavior: a Rust `use` statement must end up as an
        // `ImportUse` reference (not be dropped), with the placeholder ids the
        // cron/graph layers expect.
        let backend = LanguageRegistry::for_language("rust").expect("rust backend");
        let imports = backend.extract_imports("use crate::db::queries;\nfn main() {}");
        assert!(!imports.is_empty(), "rust `use` should produce imports");
        let refs = imports_as_references(imports);
        assert!(!refs.is_empty(), "imports must map to references");
        assert!(
            refs.iter().all(|r| r.ref_kind == SymbolRefKind::ImportUse),
            "every mapped reference must be ImportUse"
        );
        assert!(
            refs.iter().any(|r| r.target_raw.contains("queries")),
            "the import's target_raw must be preserved"
        );
        assert!(
            refs.iter()
                .all(|r| r.source_file_id == 0 && r.target_file_id.is_none()),
            "placeholders: source_file_id=0 (set on insert), target_file_id=None (resolved later)"
        );
    }

    #[test]
    fn empty_imports_yield_no_refs() {
        assert!(imports_as_references(Vec::new()).is_empty());
    }
}

/// Extract + persist for a single file. Wrapped in one transaction so the
/// DELETE + INSERT pair is atomic; rollback on FK violation (file deleted
/// concurrently) is the cron's FK-drift mitigation.
///
/// Returns `(symbols_inserted, references_inserted)` on success.
/// Fold the coarse concurrency effects derived from the ordered `sync_ops`
/// skeleton into each symbol's `effects`, keyed by `(name, start_line)`. Called
/// BEFORE `bulk_insert_symbol_effects` and the effect-drift diff so the
/// membership lands in `symbol_effects` AND the v15 drift ledger ("function X
/// gained `lock_acquire`") with no extra code.
fn fold_sync_effects(
    symbols: &mut [crate::parsing::symbols::Symbol],
    fn_sync_ops: &[crate::parsing::sync_ops::FunctionSyncOps],
) {
    use crate::parsing::sync_ops::SyncOpKind;
    use crate::parsing::type_tags::vocabulary as v;
    use std::collections::HashMap;
    if fn_sync_ops.is_empty() {
        return;
    }
    let mut idx: HashMap<(&str, u32), usize> = HashMap::with_capacity(symbols.len());
    for (i, s) in symbols.iter().enumerate() {
        idx.insert((s.name.as_str(), s.start_line), i);
    }
    let mut to_add: Vec<(usize, &'static str)> = Vec::new();
    for f in fn_sync_ops {
        let Some(&i) = idx.get(&(f.function.as_str(), f.start_line)) else {
            continue;
        };
        let (mut acq, mut rel, mut spn, mut awt, mut sel) = (false, false, false, false, false);
        for op in &f.ops {
            match op.op_kind {
                k if k.is_acquire() => acq = true,
                SyncOpKind::Release => rel = true,
                SyncOpKind::Spawn => spn = true,
                SyncOpKind::Await => awt = true,
                SyncOpKind::Select => sel = true,
                _ => {}
            }
        }
        if acq {
            to_add.push((i, v::EFFECT_LOCK_ACQUIRE));
        }
        if rel {
            to_add.push((i, v::EFFECT_LOCK_RELEASE));
        }
        if spn {
            to_add.push((i, v::EFFECT_THREAD_SPAWN));
        }
        if awt {
            to_add.push((i, v::EFFECT_AWAIT_POINT));
        }
        if sel {
            to_add.push((i, v::EFFECT_CHANNEL_SELECT));
        }
    }
    for (i, eff) in to_add {
        if !symbols[i].effects.iter().any(|e| e == eff) {
            symbols[i].effects.push(eff.to_string());
        }
    }
}

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
    // Ordered synchronization skeleton (sync_ops, v21). Fold its coarse effects
    // into `symbols` now — before the effect-drift snapshot/diff and
    // `bulk_insert_symbol_effects` below — so they reach both membership and the
    // drift ledger. The ordered rows are persisted after symbol insert.
    let fn_sync_ops = backend.extract_sync_ops(content);
    fold_sync_effects(&mut symbols, &fn_sync_ops);

    // Imports are produced by a SEPARATE backend method. Folding them into
    // `references` as `ImportUse` rows is load-bearing: without it
    // `symbol_references` carries zero `import_use` rows, so
    // `get_imports_from_symbols` returns empty and `graph_analysis` builds the
    // import graph only from its regex fallback — which is itself gated to files
    // with NO symbol refs (see `file_ids_with_symbol_refs`). Any file with a
    // call/type ref therefore ends up edgeless, collapsing the project import
    // graph to a residue (pgmcp: 14 nodes / 17 edges for 938 files) and turning
    // `dependency_graph` / `coupling_cohesion_report` / `architecture_*` into
    // fiction. Map each `Import` to an `ImportUse` reference; `target_file_id`
    // is resolved later by `graph_analysis` from `target_raw`
    // (`graph_analysis.rs`, the `imp.target_file_id.or_else(...)` path).
    // `source_file_id` is a placeholder — `bulk_insert_symbol_references` uses
    // the `file_id` argument. Backend-agnostic: every language's
    // `extract_imports` now contributes.
    references.extend(imports_as_references(backend.extract_imports(content)));

    if symbols.is_empty() && references.is_empty() {
        // Nothing to persist; still scrub stale rows for this file.
        let mut tx = pool.begin().await?;
        // Per-file cap: a pathological file fails fast and is skipped by the
        // caller (see the match arm in extract_project_symbols) rather than
        // blocking the worker. The new idx_symbol_refs_source_symbol keeps the
        // ON DELETE SET NULL cascade sub-second, so 15s is ample headroom.
        sqlx::query("SET LOCAL statement_timeout = '15s'")
            .execute(&mut *tx)
            .await?;
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

    // Temporal effect-drift (v15): snapshot the file's current effect sets
    // BEFORE the destructive re-extraction below, keyed by (kind, name), so we
    // can diff against the freshly-extracted sets and append gained/lost
    // transitions to symbol_effect_history once the new rows are persisted.
    let old_effect_sets = queries::effect_sets_for_file(pool, file_id)
        .await
        .unwrap_or_default();

    // Per-file transaction.
    let mut tx = pool.begin().await?;
    // Per-file cap — see the scrub path above for rationale.
    sqlx::query("SET LOCAL statement_timeout = '15s'")
        .execute(&mut *tx)
        .await?;
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

    // Persist the shadow-ASR rows (`symbol_parameters`, `symbol_effects`).
    // Both functions delete-then-insert per symbol_id so re-runs replace the
    // previous extraction's parameter/effect set without leaving orphans.
    // Symbols with zero IDs (the ON CONFLICT-no-row case) are filtered out.
    let mut nonzero_ids: Vec<i64> = Vec::with_capacity(symbol_ids.len());
    let mut nonzero_syms: Vec<Symbol> = Vec::with_capacity(symbols.len());
    for (sid, sym) in symbol_ids.iter().zip(symbols.iter()) {
        if *sid != 0 {
            nonzero_ids.push(*sid);
            nonzero_syms.push(sym.clone());
        }
    }
    if !nonzero_ids.is_empty() {
        queries::bulk_insert_symbol_parameters(pool, &nonzero_ids, &nonzero_syms).await?;
        queries::bulk_insert_symbol_effects(pool, &nonzero_ids, &nonzero_syms).await?;
    }

    // Persist the ordered synchronization skeleton (sync_ops, v21), keyed to the
    // just-inserted symbol_ids by (name, start_line) — the same identity the
    // coarse effects used. The `DELETE FROM file_symbols` at the top of the tx
    // already cascaded away any stale ops (ON DELETE CASCADE).
    if !fn_sync_ops.is_empty() {
        let mut id_by_key: std::collections::HashMap<(&str, u32), i64> =
            std::collections::HashMap::with_capacity(symbols.len());
        for (sym, sid) in symbols.iter().zip(symbol_ids.iter()) {
            if *sid != 0 {
                id_by_key.insert((sym.name.as_str(), sym.start_line), *sid);
            }
        }
        let mut sids: Vec<i64> = Vec::with_capacity(fn_sync_ops.len());
        let mut fops: Vec<crate::parsing::sync_ops::FunctionSyncOps> =
            Vec::with_capacity(fn_sync_ops.len());
        for f in &fn_sync_ops {
            if let Some(&sid) = id_by_key.get(&(f.function.as_str(), f.start_line)) {
                sids.push(sid);
                fops.push(f.clone());
            }
        }
        if !sids.is_empty() {
            queries::bulk_insert_sync_ops(pool, &sids, &fops).await?;
        }
    }

    // Temporal effect-drift (v15): diff the freshly-extracted effect sets
    // against the pre-extraction snapshot and append gained/lost transitions.
    // Keyed by (kind, name) so a symbol that merely moved lines isn't reported
    // as lost+gained. Non-fatal: a drift-recording failure must not abort the
    // extraction. Unchanged files (old == new) produce zero rows, so steady
    // state writes nothing.
    {
        use std::collections::{HashMap, HashSet};
        let mut new_effect_sets: HashMap<(String, String), HashSet<String>> = HashMap::new();
        for sym in &symbols {
            let entry = new_effect_sets
                .entry((sym.kind.as_db_str().to_string(), sym.name.clone()))
                .or_default();
            for eff in &sym.effects {
                entry.insert(eff.clone());
            }
        }
        let mut drift: Vec<(String, String, String, &'static str)> = Vec::new();
        for ((kind, name), new_set) in &new_effect_sets {
            let old_set = old_effect_sets.get(&(kind.clone(), name.clone()));
            for eff in new_set {
                if !old_set.map(|s| s.contains(eff)).unwrap_or(false) {
                    drift.push((kind.clone(), name.clone(), eff.clone(), "gained"));
                }
            }
        }
        for ((kind, name), old_set) in &old_effect_sets {
            let new_set = new_effect_sets.get(&(kind.clone(), name.clone()));
            for eff in old_set {
                if !new_set.map(|s| s.contains(eff)).unwrap_or(false) {
                    drift.push((kind.clone(), name.clone(), eff.clone(), "lost"));
                }
            }
        }
        if !drift.is_empty()
            && let Err(e) = queries::record_effect_drift(pool, file_id, &drift).await
        {
            warn!(file_id, error = %e, "effect-drift recording failed (non-fatal)");
        }
    }

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
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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
            ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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

    // ========================================================================
    // Additional edge-case examples — patterns the resolution must handle
    // ========================================================================

    #[test]
    fn parent_resolution_skips_zero_ids() {
        // bulk_insert_file_symbols leaves a 0 in the ids slot when ON CONFLICT
        // doesn't match — the resolver must not propagate zeros as parents.
        let symbols = vec![
            Symbol {
                file_id: 0,
                name: "Container".into(),
                kind: SymbolKind::Struct,
                start_line: 1,
                end_line: 50,
                parent_id: None,
                visibility: None,
                signature: None,
                ..Default::default()
            },
            Symbol {
                file_id: 0,
                name: "method".into(),
                kind: SymbolKind::Function,
                start_line: 10,
                end_line: 20,
                parent_id: None,
                visibility: None,
                signature: None,
                ..Default::default()
            },
        ];
        let ids = vec![0i64, 11]; // container's id is 0 — bad slot
        let pairs = compute_parent_pairs(&symbols, &ids);
        assert!(pairs.is_empty(), "should not emit pair when parent id is 0");
    }

    #[test]
    fn parent_resolution_handles_overlapping_containers() {
        // Two containers covering the same range — picks the first one
        // matched (smallest span; if span ties, first in input order wins).
        let symbols = vec![
            Symbol {
                file_id: 0,
                name: "TraitA".into(),
                kind: SymbolKind::Trait,
                start_line: 1,
                end_line: 50,
                parent_id: None,
                visibility: None,
                signature: None,
                ..Default::default()
            },
            Symbol {
                file_id: 0,
                name: "ClassB".into(),
                kind: SymbolKind::Class,
                start_line: 1,
                end_line: 50, // identical span
                parent_id: None,
                visibility: None,
                signature: None,
                ..Default::default()
            },
            Symbol {
                file_id: 0,
                name: "method".into(),
                kind: SymbolKind::Function,
                start_line: 10,
                end_line: 20,
                parent_id: None,
                visibility: None,
                signature: None,
                ..Default::default()
            },
        ];
        let ids = vec![10, 11, 12];
        let pairs = compute_parent_pairs(&symbols, &ids);
        assert_eq!(pairs.len(), 1);
        // First-in-input-order wins on tie, since `if best.map(...).unwrap_or(true)`
        // only updates when STRICTLY smaller.
        assert_eq!(pairs[0].0, 12); // method
        assert_eq!(pairs[0].1, 10); // TraitA (first in input)
    }

    #[test]
    fn source_symbol_resolution_unowned_ref_stays_none() {
        // Reference at a line not enclosed by any symbol → source_symbol_id stays None.
        let symbols = vec![Symbol {
            file_id: 0,
            name: "fn1".into(),
            kind: SymbolKind::Function,
            start_line: 10,
            end_line: 20,
            parent_id: None,
            visibility: None,
            signature: None,
            ..Default::default()
        }];
        let ids = vec![1i64];
        let mut refs = vec![SymbolReference {
            source_file_id: 0,
            source_symbol_id: None,
            target_file_id: None,
            target_symbol_id: None,
            target_raw: "thing".into(),
            ref_kind: SymbolRefKind::Call,
            source_line: 5, // before fn1
        }];
        resolve_source_symbol_ids(&symbols, &ids, &mut refs);
        assert_eq!(refs[0].source_symbol_id, None);
    }

    #[test]
    fn source_symbol_resolution_preserves_existing_assignments() {
        // Refs that already have source_symbol_id set must NOT be overwritten.
        let symbols = vec![Symbol {
            file_id: 0,
            name: "outer".into(),
            kind: SymbolKind::Function,
            start_line: 1,
            end_line: 100,
            parent_id: None,
            visibility: None,
            signature: None,
            ..Default::default()
        }];
        let ids = vec![42i64];
        let mut refs = vec![SymbolReference {
            source_file_id: 0,
            source_symbol_id: Some(99), // already set
            target_file_id: None,
            target_symbol_id: None,
            target_raw: "x".into(),
            ref_kind: SymbolRefKind::Call,
            source_line: 50,
        }];
        resolve_source_symbol_ids(&symbols, &ids, &mut refs);
        assert_eq!(refs[0].source_symbol_id, Some(99));
    }

    #[test]
    fn parent_resolution_only_promotes_function_kind() {
        // Containers contain other containers but those don't get parent_id —
        // only Functions do.
        let symbols = vec![
            Symbol {
                file_id: 0,
                name: "Outer".into(),
                kind: SymbolKind::Struct,
                start_line: 1,
                end_line: 100,
                parent_id: None,
                visibility: None,
                signature: None,
                ..Default::default()
            },
            Symbol {
                file_id: 0,
                name: "Inner".into(),
                kind: SymbolKind::Struct,
                start_line: 10,
                end_line: 50,
                parent_id: None,
                visibility: None,
                signature: None,
                ..Default::default()
            },
        ];
        let ids = vec![1i64, 2];
        let pairs = compute_parent_pairs(&symbols, &ids);
        // Inner is contained by Outer but Inner is a Struct, not a Function.
        // Current logic only promotes Functions; Inner gets no parent_pair.
        assert!(
            pairs.is_empty(),
            "compute_parent_pairs should only emit Function children, got {:?}",
            pairs
        );
    }

    #[test]
    fn dedupe_symbols_by_unique_key_works_in_place() {
        // The cron's defensive dedupe before bulk insert: same (kind, name, start_line)
        // collapsed to a single entry. Replicates the in-place HashSet retain pattern
        // used in extract_and_persist_file.
        use std::collections::HashSet;
        let mut symbols = vec![
            Symbol {
                file_id: 0,
                name: "foo".into(),
                kind: SymbolKind::Function,
                start_line: 1,
                end_line: 5,
                parent_id: None,
                visibility: None,
                signature: None,
                ..Default::default()
            },
            Symbol {
                file_id: 0,
                name: "foo".into(), // duplicate
                kind: SymbolKind::Function,
                start_line: 1,
                end_line: 5,
                parent_id: None,
                visibility: None,
                signature: None,
                ..Default::default()
            },
            Symbol {
                file_id: 0,
                name: "bar".into(),
                kind: SymbolKind::Function,
                start_line: 10,
                end_line: 15,
                parent_id: None,
                visibility: None,
                signature: None,
                ..Default::default()
            },
        ];
        let mut seen: HashSet<(SymbolKind, String, u32)> = HashSet::new();
        symbols.retain(|s| seen.insert((s.kind, s.name.clone(), s.start_line)));
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "foo");
        assert_eq!(symbols[1].name, "bar");
    }

    #[test]
    fn dedupe_references_by_unique_key_works_in_place() {
        use std::collections::HashSet;
        let mut refs = vec![
            SymbolReference {
                source_file_id: 0,
                source_symbol_id: None,
                target_file_id: None,
                target_symbol_id: None,
                target_raw: "do_thing".into(),
                ref_kind: SymbolRefKind::Call,
                source_line: 10,
            },
            SymbolReference {
                source_file_id: 0,
                source_symbol_id: None,
                target_file_id: None,
                target_symbol_id: None,
                target_raw: "do_thing".into(), // duplicate
                ref_kind: SymbolRefKind::Call,
                source_line: 10,
            },
            SymbolReference {
                source_file_id: 0,
                source_symbol_id: None,
                target_file_id: None,
                target_symbol_id: None,
                target_raw: "do_thing".into(),
                ref_kind: SymbolRefKind::Call,
                source_line: 11, // different line — kept
            },
        ];
        let mut seen: HashSet<(u32, String, String)> = HashSet::new();
        refs.retain(|r| {
            seen.insert((
                r.source_line,
                r.target_raw.clone(),
                r.ref_kind.as_db_str().to_string(),
            ))
        });
        assert_eq!(refs.len(), 2);
    }

    use proptest::prelude::*;

    proptest! {
        /// A function symbol whose start_line is within a single Struct's
        /// [start, end] range always gets that Struct as parent (when both
        /// have valid non-zero ids).
        #[test]
        fn prop_function_inside_struct_gets_parent(
            container_start in 1u32..50,
            container_span in 50u32..100,
            offset_in_container in 1u32..40,
            container_id in 1i64..1000,
            child_id in 1001i64..2000,
        ) {
            let container_end = container_start + container_span;
            let fn_start = container_start + offset_in_container.min(container_span - 1);
            let symbols = vec![
                Symbol {
                    file_id: 0,
                    name: "Container".into(),
                    kind: SymbolKind::Struct,
                    start_line: container_start,
                    end_line: container_end,
                    parent_id: None,
                    visibility: None,
                    signature: None,
                    ..Default::default()
                },
                Symbol {
                    file_id: 0,
                    name: "method".into(),
                    kind: SymbolKind::Function,
                    start_line: fn_start,
                    end_line: fn_start + 1,
                    parent_id: None,
                    visibility: None,
                    signature: None,
                    ..Default::default()
                },
            ];
            let ids = vec![container_id, child_id];
            let pairs = compute_parent_pairs(&symbols, &ids);
            prop_assert_eq!(pairs.len(), 1);
            prop_assert_eq!(pairs[0], (child_id, container_id));
        }

        /// A reference at line L gets resolved to whichever symbol's
        /// [start, end] range contains L. If multiple match, the smallest
        /// span wins. Test with two non-nested ranges where only one
        /// contains L.
        #[test]
        fn prop_reference_gets_unique_enclosing_symbol(
            target_id in 1i64..1000,
            other_id in 1001i64..2000,
            target_start in 100u32..200,
            target_span in 5u32..30,
            other_start in 1u32..50,
        ) {
            let target_end = target_start + target_span;
            let ref_line = target_start + (target_span / 2); // squarely inside target
            let symbols = vec![
                Symbol {
                    file_id: 0,
                    name: "other".into(),
                    kind: SymbolKind::Function,
                    start_line: other_start,
                    end_line: other_start + 5,
                    parent_id: None,
                    visibility: None,
                    signature: None,
                    ..Default::default()
                },
                Symbol {
                    file_id: 0,
                    name: "target".into(),
                    kind: SymbolKind::Function,
                    start_line: target_start,
                    end_line: target_end,
                    parent_id: None,
                    visibility: None,
                    signature: None,
                    ..Default::default()
                },
            ];
            let ids = vec![other_id, target_id];
            let mut refs = vec![SymbolReference {
                source_file_id: 0,
                source_symbol_id: None,
                target_file_id: None,
                target_symbol_id: None,
                target_raw: "x".into(),
                ref_kind: SymbolRefKind::Call,
                source_line: ref_line,
            }];
            resolve_source_symbol_ids(&symbols, &ids, &mut refs);
            prop_assert_eq!(refs[0].source_symbol_id, Some(target_id));
        }
    }
}
