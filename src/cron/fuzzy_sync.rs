//! Cron entry point: refresh the disk-backed `FuzzyIndex` instances
//! (symbols, paths, commits, durable_mandates) from PostgreSQL.
//!
//! Each trie is opened (or created) at
//! `$data_dir/fuzzy/{kind}/{project_slug}/{kind}.artrie`, rebuilt from
//! the PG canonical tables, then dropped. The PARChar's WAL+mmap
//! semantics keep readers from seeing torn state; opening the same
//! path concurrently is safe under the trie's TLA+-verified recovery.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use libdictenstein::DictionaryValue;
use libdictenstein::persistent_artrie::eviction::EvictionConfig;
use sqlx::PgPool;

use crate::fuzzy::persistent_artrie::{FuzzyError, FuzzyIndex};
use crate::fuzzy::sync;
use crate::fuzzy::values::{CommitRef, ConceptValue, DurableMandateRef, PathValue, SymbolValue};
use crate::stats::tracker::StatsTracker;

/// Build the canonical filesystem path for a per-project trie.
pub fn trie_path(data_dir: &Path, kind: &str, project_slug: &str) -> PathBuf {
    let mut p = data_dir.to_path_buf();
    p.push("fuzzy");
    p.push(kind);
    p.push(project_slug);
    p.push(format!("{kind}.artrie"));
    p
}

/// Filesystem path for the workspace-global concept trie — one file across all
/// projects + workspace rollups (concepts are global, like durable mandates).
/// Cached under [`CONCEPT_TRIE_SLUG`].
pub fn concept_trie_path(data_dir: &Path) -> PathBuf {
    data_dir.join("fuzzy").join("concepts_global.artrie")
}

/// Cache slug for the single global concept-trie handle in `FuzzyCache`.
pub const CONCEPT_TRIE_SLUG: &str = "_global";

/// On-disk format generation for the fuzzy ARTrie indices. **BUMP this whenever
/// the libdictenstein on-disk trie format changes incompatibly.** The 2026-06
/// lock-free *overlay* refactor (the trie now owns its concurrency; the old
/// `Arc<RwLock<…>>`-era on-disk layout is not readable by the new code) is such a
/// change — so existing `.artrie` files must be discarded and rebuilt from
/// PostgreSQL (the canonical source) rather than mis-read.
pub const FUZZY_FORMAT_VERSION: &str = "2-overlay-2026-06";

/// Ensure the on-disk fuzzy index format matches this binary. Reads the
/// `$data_dir/fuzzy/.format_version` sentinel; when it is absent or stale (an
/// upgrade across an incompatible [`FUZZY_FORMAT_VERSION`]), the ENTIRE
/// `$data_dir/fuzzy/` tree is removed and the sentinel rewritten — the
/// `fuzzy-sync` cron then repopulates every trie from PG. Returns `Ok(true)` iff
/// an existing index tree was wiped (so the caller can log the rebuild). Called
/// once at daemon startup, before any trie is opened, so the new binary never
/// opens a stale-format file. Idempotent: a matching sentinel is a cheap no-op;
/// a fresh install (no tree) just stamps the sentinel.
pub fn ensure_fuzzy_format_version(data_dir: &Path) -> std::io::Result<bool> {
    let fuzzy_root = data_dir.join("fuzzy");
    let sentinel = fuzzy_root.join(".format_version");
    if std::fs::read_to_string(&sentinel).ok().as_deref() == Some(FUZZY_FORMAT_VERSION) {
        return Ok(false);
    }
    let had_existing = fuzzy_root.exists();
    if had_existing {
        std::fs::remove_dir_all(&fuzzy_root)?;
    }
    std::fs::create_dir_all(&fuzzy_root)?;
    std::fs::write(&sentinel, FUZZY_FORMAT_VERSION)?;
    Ok(had_existing)
}

// ── Per-trie data-change gate + rebuild-fresh (the durable stale-accumulation
//    fix) ────────────────────────────────────────────────────────────────────
//
// `sync::rebuild_*` UPSERT the current source terms into the existing trie but
// never remove terms that vanished from the source, so a shrinking source (files
// deleted, a project re-scoped) left the trie retaining every term ever seen —
// the `default` symbols trie reached 11.5 GB while its live source held ~208 K
// symbols, and each `open_or_create` reopen eager-loaded that bloated image into
// heap → OOM. The fix rebuilds each trie from a CLEAN on-disk slate so it holds
// only the live source, gated on a cheap PG-source fingerprint so an unchanged
// trie is skipped (we don't rewrite every trie every run). Mirrors the proven
// `memory_graph_refresh` gate (`is_unchanged` / watermark over `pgmcp_metadata`).

/// The five fuzzy trie kinds, each with a distinct canonical PG source used by
/// the data-change gate. Per-project kinds (`Symbols`/`Paths`/`Commits`)
/// fingerprint a project-scoped source; the workspace-global kinds
/// (`DurableMandates`/`Concepts`) fingerprint a global source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FuzzyTrieKind {
    Symbols,
    Paths,
    Commits,
    DurableMandates,
    Concepts,
}

impl FuzzyTrieKind {
    /// Stable identifier used in the `pgmcp_metadata` watermark key
    /// (`fuzzy_sync:{as_str}[:{project_id}]`) and in log lines. Renaming just
    /// resets that trie's gate (a one-time forced rebuild) — never incorrect, but
    /// avoid churn.
    fn as_str(self) -> &'static str {
        match self {
            FuzzyTrieKind::Symbols => "symbols",
            FuzzyTrieKind::Paths => "paths",
            FuzzyTrieKind::Commits => "commits",
            FuzzyTrieKind::DurableMandates => "durable_mandates",
            FuzzyTrieKind::Concepts => "concepts",
        }
    }
}

/// `pgmcp_metadata` watermark key for one trie's data-change gate. Per-project
/// tries key on `fuzzy_sync:{kind}:{project_id}`; the workspace-global tries key
/// on `fuzzy_sync:{kind}` (no project scope).
fn watermark_key(kind: FuzzyTrieKind, project_id: Option<i32>) -> String {
    match project_id {
        Some(pid) => format!("fuzzy_sync:{}:{pid}", kind.as_str()),
        None => format!("fuzzy_sync:{}", kind.as_str()),
    }
}

/// The source-corpus fingerprint `"{count}:{max_id}"` plus the DB clock, in one
/// round-trip (mirrors `memory_graph_refresh::corpus_fingerprint`). `count(*)`
/// advances on every DELETE and `max(id)` on every INSERT, so BOTH a growing and
/// a *shrinking* source (the stale-accumulation trigger) move the fingerprint and
/// force a rebuild. `begin_heavy` lifts the statement timeout because `count(*)`
/// over the symbol join can exceed the pool default on a large project; the
/// oversize guard already short-circuits BEFORE this for a pathological project,
/// so the heavy count is only paid where the trie is actually eligible to rebuild.
async fn source_fingerprint(
    pool: &PgPool,
    kind: FuzzyTrieKind,
    project_id: Option<i32>,
) -> Result<(String, i64), FuzzyError> {
    let mut tx = crate::db::pool::begin_heavy(pool, "120s", "fuzzy-sync")
        .await
        .map_err(|e| FuzzyError::Trie(format!("fuzzy fingerprint begin_heavy: {e}")))?;
    let row: (String, i64) = match (kind, project_id) {
        (FuzzyTrieKind::Symbols, Some(pid)) => {
            sqlx::query_as::<_, (String, i64)>(
                "SELECT count(*)::text || ':' || coalesce(max(fs.id), 0)::text,
                        extract(epoch FROM now())::bigint
                   FROM file_symbols fs
                   JOIN indexed_files f ON fs.file_id = f.id
                  WHERE f.project_id = $1",
            )
            .bind(pid)
            .fetch_one(&mut *tx)
            .await
        }
        (FuzzyTrieKind::Paths, Some(pid)) => {
            sqlx::query_as::<_, (String, i64)>(
                "SELECT count(*)::text || ':' || coalesce(max(id), 0)::text,
                        extract(epoch FROM now())::bigint
                   FROM indexed_files
                  WHERE project_id = $1",
            )
            .bind(pid)
            .fetch_one(&mut *tx)
            .await
        }
        (FuzzyTrieKind::Commits, Some(pid)) => {
            sqlx::query_as::<_, (String, i64)>(
                "SELECT count(*)::text || ':' || coalesce(max(id), 0)::text,
                        extract(epoch FROM now())::bigint
                   FROM git_commits
                  WHERE project_id = $1",
            )
            .bind(pid)
            .fetch_one(&mut *tx)
            .await
        }
        (FuzzyTrieKind::DurableMandates, None) => {
            sqlx::query_as::<_, (String, i64)>(
                "SELECT count(*)::text || ':' || coalesce(max(id), 0)::text,
                        extract(epoch FROM now())::bigint
                   FROM durable_mandates",
            )
            .fetch_one(&mut *tx)
            .await
        }
        (FuzzyTrieKind::Concepts, None) => {
            sqlx::query_as::<_, (String, i64)>(
                "SELECT count(*)::text || ':' || coalesce(max(e.id), 0)::text,
                        extract(epoch FROM now())::bigint
                   FROM ontology_concept_meta m
                   JOIN memory_entities e ON e.id = m.entity_id AND e.valid_to IS NULL",
            )
            .fetch_one(&mut *tx)
            .await
        }
        // A per-project kind must carry a project_id and a global kind must not;
        // the call sites always pair them correctly, so an inverted pairing is a
        // programming error — refuse loudly (tx drops → rolls back) rather than
        // fingerprint the wrong set.
        (kind, project_id) => {
            return Err(FuzzyError::Trie(format!(
                "fuzzy fingerprint: invalid (kind, project) pairing {kind:?} / {project_id:?}"
            )));
        }
    }
    .map_err(|e| FuzzyError::Trie(format!("fuzzy fingerprint query: {e}")))?;
    tx.commit()
        .await
        .map_err(|e| FuzzyError::Trie(format!("fuzzy fingerprint commit: {e}")))?;
    Ok(row)
}

/// Read a trie's stored watermark (`"{fingerprint}|{epoch_secs}"`), if any.
async fn read_watermark(pool: &PgPool, key: &str) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>("SELECT value FROM pgmcp_metadata WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await
}

/// Stamp a trie's watermark after a successful rebuild.
async fn write_watermark(
    pool: &PgPool,
    key: &str,
    fingerprint: &str,
    at: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(key)
    .bind(format!("{fingerprint}|{at}"))
    .execute(pool)
    .await?;
    Ok(())
}

/// Pure gate decision (testable without a DB), identical in shape to
/// `memory_graph_refresh::is_unchanged`: the source counts as unchanged iff the
/// stored fingerprint matches the current one **and** the last rebuild is younger
/// than `max_staleness_secs`. A malformed / missing watermark, a fingerprint
/// mismatch, or an expired watermark all fall through to a rebuild.
fn is_unchanged(
    stored: Option<&str>,
    fingerprint: &str,
    now: i64,
    max_staleness_secs: u64,
) -> bool {
    let Some(stored) = stored else {
        return false;
    };
    let Some((last_fp, last_at)) = stored.split_once('|') else {
        return false;
    };
    if last_fp != fingerprint {
        return false;
    }
    let Ok(last_at) = last_at.parse::<i64>() else {
        return false;
    };
    now.saturating_sub(last_at) < max_staleness_secs as i64
}

/// A watermark staged by [`gate_trie`] to be stamped ONLY after the caller's
/// rebuild succeeds (via [`commit_watermark`]), so a failed rebuild re-attempts
/// next run instead of being masked as "unchanged".
struct PendingWatermark {
    key: String,
    fingerprint: String,
    at: i64,
}

/// Stamp a staged watermark (post-successful-rebuild).
async fn commit_watermark(pool: &PgPool, w: &PendingWatermark) -> Result<(), FuzzyError> {
    write_watermark(pool, &w.key, &w.fingerprint, w.at)
        .await
        .map_err(|e| FuzzyError::Trie(format!("fuzzy watermark write: {e}")))
}

/// Remove a trie's on-disk files so the next `open_or_create` recreates it EMPTY,
/// reflecting only the current PG source. Two on-disk layouts exist:
///
/// * **Per-project** tries (`shared_dir = false`) live alone in a dedicated
///   directory — `.../{kind}/{key}/{kind}.artrie` plus its `.wal` sidecar and the
///   generic `wal_pending/` / `wal_archive/` subdirs — so the whole PARENT dir is
///   removed (`remove_dir_all`).
/// * The two **workspace-global** tries (`shared_dir = true`: durable-mandates +
///   concepts) live DIRECTLY in the shared `$data_dir/fuzzy/` dir next to each
///   other, the `.format_version` sentinel, and a shared `wal_pending/` /
///   `wal_archive/`. Removing that dir would nuke the sibling trie and the
///   sentinel, so we remove ONLY this trie's own files — every regular file whose
///   name starts with the `.artrie` file-stem + `'.'` (e.g. `mandates_durable.` →
///   its `.artrie`, `.wal`, and any crash-left `.compacting` /
///   `.wal.compacting-stale` siblings). The generic shared `wal_pending` /
///   `wal_archive` dirs do not match the stem and are left intact; they are never
///   replayed into the freshly *created* trie (WAL recovery/replay happens only on
///   `open`, not `create`), so a clean logical slate is still guaranteed.
fn reset_trie_on_disk(artrie_path: &Path, shared_dir: bool) -> std::io::Result<()> {
    if shared_dir {
        let (Some(dir), Some(stem)) = (artrie_path.parent(), artrie_path.file_stem()) else {
            return Ok(());
        };
        let mut prefix = stem.to_os_string();
        prefix.push(".");
        let prefix = prefix.to_string_lossy().into_owned();
        match std::fs::read_dir(dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    if entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(prefix.as_str())
                        && entry.file_type().map(|t| t.is_file()).unwrap_or(false)
                    {
                        remove_file_ignore_notfound(&entry.path())?;
                    }
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    } else if let Some(dir) = artrie_path.parent() {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    } else {
        Ok(())
    }
}

/// `remove_file` that treats an already-absent file as success.
fn remove_file_ignore_notfound(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Per-trie data-change gate + rebuild-fresh reset. Returns `Ok(None)` to SKIP
/// (the PG source is unchanged since the last successful rebuild AND that rebuild
/// is within `max_staleness_secs` — the on-disk trie is left untouched), or
/// `Ok(Some(PendingWatermark))` to REBUILD. On a rebuild decision it has ALREADY
/// reset the on-disk trie to a clean slate (see [`reset_trie_on_disk`]) so the
/// caller's `open_or_create` recreates it empty. The caller MUST
/// [`commit_watermark`] the returned watermark, but only AFTER a successful
/// rebuild — so a rebuild failure re-attempts next run rather than being masked
/// as "unchanged".
async fn gate_trie(
    pool: &PgPool,
    artrie_path: &Path,
    shared_dir: bool,
    kind: FuzzyTrieKind,
    project_id: Option<i32>,
    max_staleness_secs: u64,
    report: &mut FuzzySyncReport,
) -> Result<Option<PendingWatermark>, FuzzyError> {
    let (fingerprint, now) = source_fingerprint(pool, kind, project_id).await?;
    let key = watermark_key(kind, project_id);
    let stored = read_watermark(pool, &key)
        .await
        .map_err(|e| FuzzyError::Trie(format!("fuzzy watermark read: {e}")))?;
    if is_unchanged(stored.as_deref(), &fingerprint, now, max_staleness_secs) {
        // ADR-021: an unchanged trie is an expected, by-design no-op (the gate's
        // whole purpose is to skip it), so this is info!, not error!.
        tracing::info!(
            job = "fuzzy-sync",
            kind = kind.as_str(),
            project_id = ?project_id,
            "fuzzy source unchanged since last rebuild; skipping trie rebuild"
        );
        report.skipped_unchanged += 1;
        return Ok(None);
    }
    // Rebuilding: reset the on-disk trie to a clean slate BEFORE it is reopened so
    // it reflects only the current source. Reader-safety: an existing FuzzyCache
    // reader keeps its mmap of the old inode (an mmap survives unlink), so it goes
    // on serving the prior image; only a reader that OPENS during the sub-second,
    // gated rebuild sees the freshly-emptied trie briefly — acceptable for the
    // best-effort fuzzy leg (we deliberately do NOT build-to-temp + atomically
    // swap; keep it simple per the fix's Occam constraint). The FuzzyCache's
    // mtime-staleness check re-opens the new inode on the next call.
    reset_trie_on_disk(artrie_path, shared_dir)
        .map_err(|e| FuzzyError::Trie(format!("reset {} trie: {e}", kind.as_str())))?;
    Ok(Some(PendingWatermark {
        key,
        fingerprint,
        at: now,
    }))
}

/// Run the fuzzy-sync job once across every active project.
///
/// `data_dir` is the root of the trie storage layout
/// (typically `$XDG_STATE_HOME/pgmcp/`).
pub async fn run_fuzzy_sync(
    pool: &PgPool,
    data_dir: &Path,
    max_disk_bytes: u64,
    eviction_cfg: EvictionConfig,
    checkpoint_every_rows: usize,
    oversize_threshold: u64,
    max_staleness_secs: u64,
    stats: Arc<StatsTracker>,
) -> Result<FuzzySyncReport, FuzzyError> {
    let mut report = FuzzySyncReport::default();

    // Enumerate projects. Each project gets its own per-kind trie file.
    let projects: Vec<(i32, String)> =
        sqlx::query_as::<_, (i32, String)>("SELECT id, name FROM projects ORDER BY id")
            .fetch_all(pool)
            .await
            .map_err(|e| FuzzyError::Trie(format!("project list: {e}")))?;

    for (project_id, project_name) in &projects {
        let project_key = project_artifact_key(*project_id, project_name);

        // Skip-oversize guard (the reliable, active OOM fix): a project whose
        // source exceeds `oversize_threshold` rows would build a pathologically
        // large trie ENTIRELY in RAM (the `default` project's 11.5 GB symbols
        // trie is what OOM'd the daemon). When over the cap we do NOT open or
        // build the trie — the prior on-disk trie is left untouched and readers
        // keep serving it. Each kind is guarded independently: paths/commits are
        // usually well under the cap even when symbols blow past it.
        //
        // Eviction MUST be enabled BEFORE the rebuild: the per-page checkpoints in
        // `rebuild_*` only bound the overlay (swizzle cold nodes to disk down to
        // `resident_budget_bytes`) when the coordinator is already installed. This
        // is the crux of the incremental-checkpoint OOM fix — before it, eviction
        // was enabled in `finalize_trie` AFTER the whole trie was built in RAM.
        if !should_skip_oversize(
            pool,
            *project_id,
            project_name,
            sync::FuzzySource::Symbols,
            "symbols",
            oversize_threshold,
            &mut report,
        )
        .await?
        {
            let symbols_path = trie_path(data_dir, "symbols", &project_key);
            if let Some(watermark) = gate_trie(
                pool,
                &symbols_path,
                false,
                FuzzyTrieKind::Symbols,
                Some(*project_id),
                max_staleness_secs,
                &mut report,
            )
            .await?
            {
                let (sym_idx, _sym_recovery) =
                    FuzzyIndex::<SymbolValue>::open_or_create(&symbols_path)?;
                prime_eviction(&sym_idx, max_disk_bytes, &eviction_cfg);
                report.symbols_synced +=
                    sync::rebuild_symbols(pool, *project_id, &sym_idx, checkpoint_every_rows)
                        .await?;
                finalize_trie(&sym_idx, &symbols_path, max_disk_bytes, &stats)?;
                commit_watermark(pool, &watermark).await?;
            }
        }

        if !should_skip_oversize(
            pool,
            *project_id,
            project_name,
            sync::FuzzySource::Paths,
            "paths",
            oversize_threshold,
            &mut report,
        )
        .await?
        {
            let paths_path = trie_path(data_dir, "paths", &project_key);
            if let Some(watermark) = gate_trie(
                pool,
                &paths_path,
                false,
                FuzzyTrieKind::Paths,
                Some(*project_id),
                max_staleness_secs,
                &mut report,
            )
            .await?
            {
                let (path_idx, _path_recovery) =
                    FuzzyIndex::<PathValue>::open_or_create(&paths_path)?;
                prime_eviction(&path_idx, max_disk_bytes, &eviction_cfg);
                report.paths_synced +=
                    sync::rebuild_paths(pool, *project_id, &path_idx, checkpoint_every_rows)
                        .await?;
                finalize_trie(&path_idx, &paths_path, max_disk_bytes, &stats)?;
                commit_watermark(pool, &watermark).await?;
            }
        }

        if !should_skip_oversize(
            pool,
            *project_id,
            project_name,
            sync::FuzzySource::Commits,
            "commits",
            oversize_threshold,
            &mut report,
        )
        .await?
        {
            let commits_path = trie_path(data_dir, "commits", &project_key);
            if let Some(watermark) = gate_trie(
                pool,
                &commits_path,
                false,
                FuzzyTrieKind::Commits,
                Some(*project_id),
                max_staleness_secs,
                &mut report,
            )
            .await?
            {
                let (commit_idx, _commit_recovery) =
                    FuzzyIndex::<CommitRef>::open_or_create(&commits_path)?;
                prime_eviction(&commit_idx, max_disk_bytes, &eviction_cfg);
                report.commits_synced +=
                    sync::rebuild_commits(pool, *project_id, &commit_idx, checkpoint_every_rows)
                        .await?;
                finalize_trie(&commit_idx, &commits_path, max_disk_bytes, &stats)?;
                commit_watermark(pool, &watermark).await?;
            }
        }
    }

    // Durable mandates are workspace-global; one trie shared across all projects.
    // It lives DIRECTLY in `$data_dir/fuzzy/` (not a dedicated dir), so the gate
    // resets it via the shared-dir path (stem-matched files only).
    let mandates_path = data_dir.join("fuzzy").join("mandates_durable.artrie");
    if let Some(watermark) = gate_trie(
        pool,
        &mandates_path,
        true,
        FuzzyTrieKind::DurableMandates,
        None,
        max_staleness_secs,
        &mut report,
    )
    .await?
    {
        let (mandate_idx, _mandate_recovery) =
            FuzzyIndex::<DurableMandateRef>::open_or_create(&mandates_path)?;
        prime_eviction(&mandate_idx, max_disk_bytes, &eviction_cfg);
        report.durable_mandates_synced +=
            sync::rebuild_durable_mandates(pool, &mandate_idx, checkpoint_every_rows).await?;
        finalize_trie(&mandate_idx, &mandates_path, max_disk_bytes, &stats)?;
        commit_watermark(pool, &watermark).await?;
    }

    // Concepts (ontology) are workspace-global like durable mandates: one trie
    // across all projects + workspace rollups, keyed by concept name. Backs the
    // typo-tolerant / prefix legs of `ontology_search` + `{concept}` completion.
    // Also shares `$data_dir/fuzzy/`, so it uses the shared-dir reset path too.
    let concepts_path = concept_trie_path(data_dir);
    if let Some(watermark) = gate_trie(
        pool,
        &concepts_path,
        true,
        FuzzyTrieKind::Concepts,
        None,
        max_staleness_secs,
        &mut report,
    )
    .await?
    {
        let (concept_idx, _concept_recovery) =
            FuzzyIndex::<ConceptValue>::open_or_create(&concepts_path)?;
        prime_eviction(&concept_idx, max_disk_bytes, &eviction_cfg);
        report.concepts_synced +=
            sync::rebuild_concepts(pool, &concept_idx, checkpoint_every_rows).await?;
        finalize_trie(&concept_idx, &concepts_path, max_disk_bytes, &stats)?;
        commit_watermark(pool, &watermark).await?;
    }

    stats
        .fuzzy_sync_runs
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    stats.fuzzy_sync_rows_synced.fetch_add(
        (report.symbols_synced
            + report.paths_synced
            + report.commits_synced
            + report.durable_mandates_synced
            + report.concepts_synced) as u64,
        std::sync::atomic::Ordering::Relaxed,
    );

    Ok(report)
}

/// Post-rebuild finalization for one trie: enable heap eviction (when
/// `max_disk_bytes > 0`), checkpoint to persist + populate the eviction
/// registry, enforce the on-disk advisory cap, and fold the trie's eviction
/// stats into the global counters.
fn finalize_trie<V>(
    idx: &FuzzyIndex<V>,
    path: &Path,
    max_disk_bytes: u64,
    stats: &StatsTracker,
) -> Result<(), FuzzyError>
where
    V: DictionaryValue + Clone + Send + Sync + 'static,
{
    // Final checkpoint: persist any residual overlay from the last (partial) page
    // and run the resident-budget eviction tail one last time. Eviction itself was
    // enabled by `prime_eviction` BEFORE the rebuild, so the per-page checkpoints
    // already bounded RAM; this is the closing flush.
    idx.checkpoint()?;
    crate::fuzzy::disk_guard::enforce_disk_cap(path, max_disk_bytes, stats);
    crate::fuzzy::disk_guard::record_eviction_stats(idx, stats);
    Ok(())
}

/// Enable heap eviction on a freshly-opened trie BEFORE its rebuild, so the
/// per-page checkpoints in `sync::rebuild_*` bound the in-memory overlay
/// (swizzling the coldest nodes to disk down to `resident_budget_bytes`). No-op
/// when `max_disk_bytes == 0` (eviction disabled). A reused handle's "already
/// enabled" error is tolerated. This ordering — eviction before the first insert —
/// is what makes the rebuild memory-bounded (the 2026-07-08 OOM fix).
fn prime_eviction<V>(idx: &FuzzyIndex<V>, max_disk_bytes: u64, eviction_cfg: &EvictionConfig)
where
    V: DictionaryValue + Clone + Send + Sync + 'static,
{
    if max_disk_bytes > 0 {
        let _ = idx.enable_eviction(eviction_cfg.clone());
    }
}

/// Skip-oversize decision for one (project, kind): returns `true` — and logs +
/// counts the skip — when the project's source for `source` exceeds
/// `threshold` rows. The caller then does NOT open or build the trie, leaving
/// the prior on-disk trie intact.
///
/// ADR-021: a **designed cap** (not a runtime failure) logs at `warn!`, not
/// `error!`. `threshold == 0` disables the guard (`source_exceeds` returns
/// `false` without querying), so this is a cheap no-op when the knob is off.
async fn should_skip_oversize(
    pool: &PgPool,
    project_id: i32,
    project_name: &str,
    source: sync::FuzzySource,
    kind: &str,
    threshold: u64,
    report: &mut FuzzySyncReport,
) -> Result<bool, FuzzyError> {
    if sync::source_exceeds(pool, project_id, source, threshold).await? {
        tracing::warn!(
            job = "fuzzy-sync",
            project_id,
            project = %project_name,
            kind,
            threshold,
            "skipping oversize fuzzy trie rebuild (source rows exceed \
             [fuzzy] oversize_trie_row_threshold); keeping prior on-disk trie"
        );
        report.skipped_oversize += 1;
        return Ok(true);
    }
    Ok(false)
}

/// Per-run summary for the fuzzy-sync cron.
#[derive(Debug, Default, Clone)]
pub struct FuzzySyncReport {
    pub symbols_synced: usize,
    pub paths_synced: usize,
    pub commits_synced: usize,
    pub durable_mandates_synced: usize,
    pub concepts_synced: usize,
    /// Number of per-(project, kind) trie rebuilds skipped by the
    /// skip-oversize guard ([`crate::config::FuzzyConfig::oversize_trie_row_threshold`]).
    pub skipped_oversize: usize,
    /// Number of (project, kind) tries skipped by the data-change gate because
    /// their PG source was unchanged since the last successful rebuild (within
    /// `[cron] fuzzy_sync_max_staleness_secs`). The on-disk trie was left intact.
    pub skipped_unchanged: usize,
}

/// Filesystem-safe project slug.
pub fn slugify(name: &str) -> String {
    let mut s = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            s.push(ch);
        } else {
            s.push('_');
        }
    }
    s
}

/// Stable per-project artifact key for fuzzy tries and HybridLM files.
///
/// Project display names are not unique and `slugify` is many-to-one
/// (`"foo/bar"` and `"foo_bar"` collide). Include the database id so every
/// indexed project gets a distinct on-disk namespace while keeping paths
/// inspectable.
pub fn project_artifact_key(project_id: i32, name: &str) -> String {
    format!("{}-p{}", slugify(name), project_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trie_path_layout_matches_documented_convention() {
        let path = trie_path(Path::new("/var/state/pgmcp"), "symbols", "pgmcp");
        assert_eq!(
            path,
            Path::new("/var/state/pgmcp/fuzzy/symbols/pgmcp/symbols.artrie")
        );
    }

    #[test]
    fn slugify_strips_unsafe_chars() {
        assert_eq!(slugify("pgmcp"), "pgmcp");
        assert_eq!(slugify("rholang-rs"), "rholang-rs");
        assert_eq!(slugify("MeTTa Compiler"), "MeTTa_Compiler");
        assert_eq!(slugify("foo/bar"), "foo_bar");
    }

    #[test]
    fn project_artifact_key_disambiguates_slug_collisions() {
        assert_eq!(project_artifact_key(7, "foo/bar"), "foo_bar-p7");
        assert_eq!(project_artifact_key(8, "foo_bar"), "foo_bar-p8");
        assert_ne!(
            project_artifact_key(7, "foo/bar"),
            project_artifact_key(8, "foo_bar")
        );
    }

    #[test]
    fn fuzzy_format_guard_stamps_fresh_wipes_stale_and_noops_on_match() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path();
        let fuzzy_root = data_dir.join("fuzzy");
        let sentinel = fuzzy_root.join(".format_version");

        // Fresh install: no existing tree → stamps the sentinel, reports no wipe.
        assert!(
            !ensure_fuzzy_format_version(data_dir).expect("fresh"),
            "fresh install does not report a wipe"
        );
        assert_eq!(
            std::fs::read_to_string(&sentinel).expect("sentinel written"),
            FUZZY_FORMAT_VERSION
        );

        // Matching sentinel → cheap no-op, leaves any contents intact.
        let marker = fuzzy_root.join("symbols").join("p1").join("symbols.artrie");
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, b"trie-bytes").unwrap();
        assert!(
            !ensure_fuzzy_format_version(data_dir).expect("match"),
            "matching version is a no-op"
        );
        assert!(marker.exists(), "no-op must not wipe existing tries");

        // Stale (incompatible old) format: a tree with a mismatched/absent
        // sentinel → wipes the whole tree and re-stamps.
        std::fs::write(&sentinel, "1-legacy-rwlock").unwrap();
        assert!(
            ensure_fuzzy_format_version(data_dir).expect("stale"),
            "stale format reports a wipe"
        );
        assert!(!marker.exists(), "stale tries are wiped for rebuild");
        assert_eq!(
            std::fs::read_to_string(&sentinel).expect("re-stamped"),
            FUZZY_FORMAT_VERSION
        );
    }

    // ── Data-change gate (`is_unchanged`) — reused verbatim from
    //    `memory_graph_refresh`'s test template (the proven gate). ────────────

    #[test]
    fn no_watermark_forces_rebuild() {
        assert!(!is_unchanged(None, "10:20", 1_000, 86_400));
    }

    #[test]
    fn matching_fingerprint_within_window_skips() {
        // stored fp matches, rebuilt 1h ago, 24h window → unchanged (skip).
        assert!(is_unchanged(Some("10:20|0"), "10:20", 3_600, 86_400));
    }

    #[test]
    fn changed_fingerprint_forces_rebuild() {
        assert!(!is_unchanged(Some("10:20|0"), "11:21", 3_600, 86_400));
    }

    #[test]
    fn shrinking_source_changes_fingerprint_and_forces_rebuild() {
        // The stale-accumulation trigger: the source shrank (count 208→207) even
        // though max_id is unchanged — count(*) moves, so the gate rebuilds.
        assert!(!is_unchanged(Some("208:999|0"), "207:999", 3_600, 86_400));
    }

    #[test]
    fn stale_watermark_forces_rebuild() {
        // fp matches but the last rebuild is older than the window → rebuild anyway.
        assert!(!is_unchanged(Some("10:20|0"), "10:20", 90_000, 86_400));
    }

    #[test]
    fn zero_staleness_window_always_rebuilds() {
        // max_staleness_secs = 0 collapses the freshness window → never skip.
        assert!(!is_unchanged(Some("10:20|0"), "10:20", 0, 0));
    }

    #[test]
    fn malformed_watermark_forces_rebuild() {
        assert!(!is_unchanged(Some("garbage"), "10:20", 1_000, 86_400));
        assert!(!is_unchanged(
            Some("10:20|notanumber"),
            "10:20",
            1_000,
            86_400
        ));
    }

    #[test]
    fn watermark_key_scopes_per_project_and_global() {
        assert_eq!(
            watermark_key(FuzzyTrieKind::Symbols, Some(7)),
            "fuzzy_sync:symbols:7"
        );
        assert_eq!(
            watermark_key(FuzzyTrieKind::Paths, Some(42)),
            "fuzzy_sync:paths:42"
        );
        assert_eq!(
            watermark_key(FuzzyTrieKind::DurableMandates, None),
            "fuzzy_sync:durable_mandates"
        );
        assert_eq!(
            watermark_key(FuzzyTrieKind::Concepts, None),
            "fuzzy_sync:concepts"
        );
    }

    // ── Rebuild-fresh reset (`reset_trie_on_disk`) — pure filesystem, no DB. ───

    #[test]
    fn reset_per_project_removes_whole_dedicated_dir_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path();
        // .../fuzzy/symbols/proj-p1/symbols.artrie (+ .wal + wal_pending/seg).
        let artrie = trie_path(data_dir, "symbols", "proj-p1");
        let dir = artrie.parent().expect("parent");
        std::fs::create_dir_all(dir.join("wal_pending")).unwrap();
        std::fs::write(&artrie, b"trie").unwrap();
        std::fs::write(dir.join("symbols.wal"), b"wal").unwrap();
        std::fs::write(dir.join("wal_pending").join("seg-1"), b"seg").unwrap();
        // A DIFFERENT project's trie dir must survive the reset.
        let sibling = trie_path(data_dir, "symbols", "other-p2");
        std::fs::create_dir_all(sibling.parent().unwrap()).unwrap();
        std::fs::write(&sibling, b"other").unwrap();

        reset_trie_on_disk(&artrie, false).expect("reset per-project");

        assert!(
            !dir.exists(),
            "the dedicated per-project dir is removed whole"
        );
        assert!(sibling.exists(), "a sibling project's trie is untouched");

        // Idempotent: a second reset on the now-absent dir is a clean no-op.
        reset_trie_on_disk(&artrie, false).expect("reset idempotent on missing dir");
    }

    #[test]
    fn reset_global_removes_only_its_stem_files_not_shared_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let fuzzy = tmp.path().join("fuzzy");
        std::fs::create_dir_all(fuzzy.join("wal_pending")).unwrap();
        // The two global tries share $data_dir/fuzzy/ with the format sentinel.
        let mandates = fuzzy.join("mandates_durable.artrie");
        std::fs::write(&mandates, b"m").unwrap();
        std::fs::write(fuzzy.join("mandates_durable.wal"), b"m").unwrap();
        std::fs::write(fuzzy.join("mandates_durable.wal.compacting-stale"), b"m").unwrap();
        let concepts = fuzzy.join("concepts_global.artrie");
        std::fs::write(&concepts, b"c").unwrap();
        std::fs::write(fuzzy.join("concepts_global.wal"), b"c").unwrap();
        std::fs::write(fuzzy.join(".format_version"), FUZZY_FORMAT_VERSION).unwrap();
        std::fs::write(fuzzy.join("wal_pending").join("seg-1"), b"s").unwrap();

        reset_trie_on_disk(&mandates, true).expect("reset global");

        // This trie's own files (stem `mandates_durable.`) are gone …
        assert!(!mandates.exists(), "global .artrie removed");
        assert!(
            !fuzzy.join("mandates_durable.wal").exists(),
            "global .wal removed"
        );
        assert!(
            !fuzzy.join("mandates_durable.wal.compacting-stale").exists(),
            "crash-left compaction sibling removed"
        );
        // … but the sibling global trie, the sentinel, and the shared wal_pending
        // dir (+ its segments) all survive.
        assert!(concepts.exists(), "sibling concepts .artrie untouched");
        assert!(
            fuzzy.join("concepts_global.wal").exists(),
            "sibling .wal untouched"
        );
        assert!(
            fuzzy.join(".format_version").exists(),
            "format sentinel preserved (would break the whole tree if removed)"
        );
        assert!(
            fuzzy.join("wal_pending").is_dir(),
            "shared wal_pending dir preserved"
        );
        assert!(
            fuzzy.join("wal_pending").join("seg-1").exists(),
            "shared wal_pending contents preserved"
        );
    }
}
