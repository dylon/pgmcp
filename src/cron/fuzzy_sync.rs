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

/// Run the fuzzy-sync job once across every active project.
///
/// `data_dir` is the root of the trie storage layout
/// (typically `$XDG_STATE_HOME/pgmcp/`).
pub async fn run_fuzzy_sync(
    pool: &PgPool,
    data_dir: &Path,
    max_disk_bytes: u64,
    eviction_cfg: EvictionConfig,
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
        let project_slug = slugify(project_name);

        let symbols_path = trie_path(data_dir, "symbols", &project_slug);
        let paths_path = trie_path(data_dir, "paths", &project_slug);
        let commits_path = trie_path(data_dir, "commits", &project_slug);

        let (sym_idx, _sym_recovery) = FuzzyIndex::<SymbolValue>::open_or_create(&symbols_path)?;
        let (path_idx, _path_recovery) = FuzzyIndex::<PathValue>::open_or_create(&paths_path)?;
        let (commit_idx, _commit_recovery) =
            FuzzyIndex::<CommitRef>::open_or_create(&commits_path)?;

        report.symbols_synced += sync::rebuild_symbols(pool, *project_id, &sym_idx).await?;
        finalize_trie(
            &sym_idx,
            &symbols_path,
            max_disk_bytes,
            &eviction_cfg,
            &stats,
        )?;
        report.paths_synced += sync::rebuild_paths(pool, *project_id, &path_idx).await?;
        finalize_trie(
            &path_idx,
            &paths_path,
            max_disk_bytes,
            &eviction_cfg,
            &stats,
        )?;
        report.commits_synced += sync::rebuild_commits(pool, *project_id, &commit_idx).await?;
        finalize_trie(
            &commit_idx,
            &commits_path,
            max_disk_bytes,
            &eviction_cfg,
            &stats,
        )?;
    }

    // Durable mandates are workspace-global; one trie shared across
    // all projects.
    let mandates_path = data_dir.join("fuzzy").join("mandates_durable.artrie");
    let (mandate_idx, _mandate_recovery) =
        FuzzyIndex::<DurableMandateRef>::open_or_create(&mandates_path)?;
    report.durable_mandates_synced += sync::rebuild_durable_mandates(pool, &mandate_idx).await?;
    finalize_trie(
        &mandate_idx,
        &mandates_path,
        max_disk_bytes,
        &eviction_cfg,
        &stats,
    )?;

    // Concepts (ontology) are workspace-global like durable mandates: one trie
    // across all projects + workspace rollups, keyed by concept name. Backs the
    // typo-tolerant / prefix legs of `ontology_search` + `{concept}` completion.
    let concepts_path = concept_trie_path(data_dir);
    let (concept_idx, _concept_recovery) =
        FuzzyIndex::<ConceptValue>::open_or_create(&concepts_path)?;
    report.concepts_synced += sync::rebuild_concepts(pool, &concept_idx).await?;
    finalize_trie(
        &concept_idx,
        &concepts_path,
        max_disk_bytes,
        &eviction_cfg,
        &stats,
    )?;

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
    eviction_cfg: &EvictionConfig,
    stats: &StatsTracker,
) -> Result<(), FuzzyError>
where
    V: DictionaryValue + Clone + Send + Sync + 'static,
{
    if max_disk_bytes > 0 {
        // A freshly-opened trie is never already-enabled; tolerate the
        // "already enabled" error rather than abort the whole sync.
        let _ = idx.enable_eviction(eviction_cfg.clone());
    }
    // Checkpoint persists the rebuilt trie and, when eviction is enabled,
    // populates the coordinator's disk-location registry so eviction can
    // reclaim in-memory node boxes under memory pressure.
    idx.checkpoint()?;
    crate::fuzzy::disk_guard::enforce_disk_cap(path, max_disk_bytes, stats);
    crate::fuzzy::disk_guard::record_eviction_stats(idx, stats);
    Ok(())
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
}
