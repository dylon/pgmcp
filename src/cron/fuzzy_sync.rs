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

use sqlx::PgPool;

use crate::fuzzy::persistent_artrie::{FuzzyError, FuzzyIndex};
use crate::fuzzy::sync;
use crate::fuzzy::values::{CommitRef, DurableMandateRef, PathValue, SymbolValue};
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

/// Run the fuzzy-sync job once across every active project.
///
/// `data_dir` is the root of the trie storage layout
/// (typically `$XDG_STATE_HOME/pgmcp/`).
pub async fn run_fuzzy_sync(
    pool: &PgPool,
    data_dir: &Path,
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
        report.paths_synced += sync::rebuild_paths(pool, *project_id, &path_idx).await?;
        report.commits_synced += sync::rebuild_commits(pool, *project_id, &commit_idx).await?;
    }

    // Durable mandates are workspace-global; one trie shared across
    // all projects.
    let mandates_path = data_dir.join("fuzzy").join("mandates_durable.artrie");
    let (mandate_idx, _mandate_recovery) =
        FuzzyIndex::<DurableMandateRef>::open_or_create(&mandates_path)?;
    report.durable_mandates_synced += sync::rebuild_durable_mandates(pool, &mandate_idx).await?;

    stats
        .fuzzy_sync_runs
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    stats.fuzzy_sync_rows_synced.fetch_add(
        (report.symbols_synced
            + report.paths_synced
            + report.commits_synced
            + report.durable_mandates_synced) as u64,
        std::sync::atomic::Ordering::Relaxed,
    );

    Ok(report)
}

/// Per-run summary for the fuzzy-sync cron.
#[derive(Debug, Default, Clone)]
pub struct FuzzySyncReport {
    pub symbols_synced: usize,
    pub paths_synced: usize,
    pub commits_synced: usize,
    pub durable_mandates_synced: usize,
}

/// Filesystem-safe project slug.
fn slugify(name: &str) -> String {
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
