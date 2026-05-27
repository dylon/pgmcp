//! On-disk size guard for the persistent fuzzy tries.
//!
//! `[fuzzy].max_disk_bytes` is an ADVISORY cap on each trie's on-disk footprint.
//! Heap eviction (libdictenstein's memory-pressure coordinator) reclaims RAM,
//! not disk, so this guard does not shrink files — it measures each trie's
//! on-disk size after a sync and logs a warning + bumps a stat when the cap is
//! exceeded, so operators can rebuild/prune. It also folds a trie's cumulative
//! eviction stats into the global counters.

use std::path::Path;
use std::sync::atomic::Ordering;

use libdictenstein::DictionaryValue;

use crate::fuzzy::persistent_artrie::FuzzyIndex;
use crate::stats::tracker::StatsTracker;

/// Sum the on-disk byte size of a trie's backing files. Per the `trie_path`
/// layout (`.../{kind}/{slug}/{kind}.artrie` plus any WAL/arena siblings), all
/// of a trie's files live in the file's parent directory, so we sum the regular
/// files in that directory.
fn trie_on_disk_bytes(path: &Path) -> u64 {
    let Some(dir) = path.parent() else {
        return 0;
    };
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata()
                && meta.is_file()
            {
                total += meta.len();
            }
        }
    }
    total
}

/// Measure the trie's on-disk size at `path`, record it in `stats`, and warn +
/// bump `fuzzy_disk_cap_exceeded` when it exceeds `max_disk_bytes` (cap > 0).
/// Returns the observed on-disk byte size.
pub fn enforce_disk_cap(path: &Path, max_disk_bytes: u64, stats: &StatsTracker) -> u64 {
    let bytes = trie_on_disk_bytes(path);
    stats.fuzzy_disk_bytes_last.store(bytes, Ordering::Relaxed);
    if max_disk_bytes > 0 && bytes > max_disk_bytes {
        stats
            .fuzzy_disk_cap_exceeded
            .fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            path = %path.display(),
            on_disk_bytes = bytes,
            cap_bytes = max_disk_bytes,
            "fuzzy trie exceeds [fuzzy].max_disk_bytes (advisory: on-disk size is \
             not shrunk online — consider rebuilding/pruning the project)"
        );
    }
    bytes
}

/// Fold a trie's cumulative eviction stats into the global counters. No-op when
/// eviction is not enabled on the trie.
pub fn record_eviction_stats<V>(idx: &FuzzyIndex<V>, stats: &StatsTracker)
where
    V: DictionaryValue + Clone + Send + Sync + 'static,
{
    if !idx.eviction_enabled() {
        return;
    }
    let ev = idx.eviction_stats();
    stats
        .fuzzy_nodes_evicted
        .fetch_add(ev.nodes_evicted, Ordering::Relaxed);
    stats
        .fuzzy_bytes_freed
        .fetch_add(ev.bytes_freed, Ordering::Relaxed);
}
