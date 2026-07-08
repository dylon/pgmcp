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

        let symbols_path = trie_path(data_dir, "symbols", &project_key);
        let paths_path = trie_path(data_dir, "paths", &project_key);
        let commits_path = trie_path(data_dir, "commits", &project_key);

        // Eviction MUST be enabled BEFORE the rebuild: the per-page checkpoints in
        // `rebuild_*` only bound the overlay (swizzle cold nodes to disk down to
        // `resident_budget_bytes`) when the coordinator is already installed. This
        // is the crux of the 2026-07-08 OOM fix — before it, eviction was enabled
        // in `finalize_trie` AFTER the whole trie was built in RAM.
        let (sym_idx, _sym_recovery) = FuzzyIndex::<SymbolValue>::open_or_create(&symbols_path)?;
        prime_eviction(&sym_idx, max_disk_bytes, &eviction_cfg);
        let (path_idx, _path_recovery) = FuzzyIndex::<PathValue>::open_or_create(&paths_path)?;
        prime_eviction(&path_idx, max_disk_bytes, &eviction_cfg);
        let (commit_idx, _commit_recovery) =
            FuzzyIndex::<CommitRef>::open_or_create(&commits_path)?;
        prime_eviction(&commit_idx, max_disk_bytes, &eviction_cfg);

        report.symbols_synced +=
            sync::rebuild_symbols(pool, *project_id, &sym_idx, checkpoint_every_rows).await?;
        finalize_trie(&sym_idx, &symbols_path, max_disk_bytes, &stats)?;
        report.paths_synced +=
            sync::rebuild_paths(pool, *project_id, &path_idx, checkpoint_every_rows).await?;
        finalize_trie(&path_idx, &paths_path, max_disk_bytes, &stats)?;
        report.commits_synced +=
            sync::rebuild_commits(pool, *project_id, &commit_idx, checkpoint_every_rows).await?;
        finalize_trie(&commit_idx, &commits_path, max_disk_bytes, &stats)?;
    }

    // Durable mandates are workspace-global; one trie shared across
    // all projects.
    let mandates_path = data_dir.join("fuzzy").join("mandates_durable.artrie");
    let (mandate_idx, _mandate_recovery) =
        FuzzyIndex::<DurableMandateRef>::open_or_create(&mandates_path)?;
    prime_eviction(&mandate_idx, max_disk_bytes, &eviction_cfg);
    report.durable_mandates_synced +=
        sync::rebuild_durable_mandates(pool, &mandate_idx, checkpoint_every_rows).await?;
    finalize_trie(&mandate_idx, &mandates_path, max_disk_bytes, &stats)?;

    // Concepts (ontology) are workspace-global like durable mandates: one trie
    // across all projects + workspace rollups, keyed by concept name. Backs the
    // typo-tolerant / prefix legs of `ontology_search` + `{concept}` completion.
    let concepts_path = concept_trie_path(data_dir);
    let (concept_idx, _concept_recovery) =
        FuzzyIndex::<ConceptValue>::open_or_create(&concepts_path)?;
    prime_eviction(&concept_idx, max_disk_bytes, &eviction_cfg);
    report.concepts_synced +=
        sync::rebuild_concepts(pool, &concept_idx, checkpoint_every_rows).await?;
    finalize_trie(&concept_idx, &concepts_path, max_disk_bytes, &stats)?;

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

/// Per-run summary for the fuzzy-sync cron.
#[derive(Debug, Default, Clone)]
pub struct FuzzySyncReport {
    pub symbols_synced: usize,
    pub paths_synced: usize,
    pub commits_synced: usize,
    pub durable_mandates_synced: usize,
    pub concepts_synced: usize,
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
}
