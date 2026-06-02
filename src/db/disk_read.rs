//! Hash-verified disk reads for files indexed under the asymmetric-storage
//! policy (`indexed_files.content IS NULL` + `content_recoverable_from_disk`).
//!
//! The production indexing path (`src/embed/pool.rs`) deliberately stores
//! `content = NULL` for plain-text files whose bytes are cheap to re-read from
//! disk, keeping only a `content_hash` (xxHash3-64). Consumers that need the
//! full text — the `read_file` MCP tool and the symbol-extraction cron —
//! recover it from disk and verify it against `content_hash`, so a file edited
//! since indexing is never silently mis-read. This module is the single shared
//! implementation of that fast-path so the two call sites cannot diverge.

use xxhash_rust::xxh3::xxh3_64;

/// Outcome of a hash-verified disk read for a content-NULL file.
pub enum DiskReadOutcome {
    /// Disk bytes read and hash-verified against `indexed_files.content_hash`.
    Hit(String),
    /// The file exists but its bytes no longer match the indexed hash (edited
    /// since indexing); the caller should fall back or skip rather than trust
    /// stale content.
    HashMismatch,
    /// The file is gone from disk (deleted/moved since indexing).
    Missing,
    /// Read failed for any other reason (permissions, non-UTF-8 bytes, …).
    IoError,
    /// Not eligible for the disk fast-path: `content_recoverable_from_disk` is
    /// false or `content_hash` is absent.
    NotRecoverable,
}

/// The canonical content hash pgmcp stores in `indexed_files.content_hash`:
/// xxHash3-64 of the bytes, reinterpreted as `i64` for Postgres `BIGINT`.
/// Centralizes the `xxh3_64(..) as i64` convention shared by the indexer
/// (`src/embed/pool.rs`) and the read/extract disk fast-paths.
pub fn content_hash_i64(bytes: &[u8]) -> i64 {
    xxh3_64(bytes) as i64
}

/// Read `path` from disk and verify it against `expected_hash`
/// (`indexed_files.content_hash`, an xxHash3-64 stored as `i64`). Mirrors the
/// `read_file` tool's disk fast-path so the two consumers cannot drift.
pub fn read_disk_verified(
    path: &str,
    recoverable: bool,
    expected_hash: Option<i64>,
) -> DiskReadOutcome {
    let Some(expected) = expected_hash else {
        return DiskReadOutcome::NotRecoverable;
    };
    if !recoverable {
        return DiskReadOutcome::NotRecoverable;
    }
    match std::fs::read_to_string(path) {
        Ok(bytes) => {
            if content_hash_i64(bytes.as_bytes()) == expected {
                DiskReadOutcome::Hit(bytes)
            } else {
                DiskReadOutcome::HashMismatch
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DiskReadOutcome::Missing,
        Err(_) => DiskReadOutcome::IoError,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "pgmcp_disk_read_{}_{}.txt",
            std::process::id(),
            tag
        ))
    }

    #[test]
    fn hit_when_hash_matches() {
        let path = scratch_path("hit");
        let body = "fn alpha() {}\n";
        std::fs::write(&path, body).expect("write scratch");
        let outcome = read_disk_verified(
            path.to_str().expect("utf8 path"),
            true,
            Some(content_hash_i64(body.as_bytes())),
        );
        let _ = std::fs::remove_file(&path);
        assert!(
            matches!(outcome, DiskReadOutcome::Hit(ref s) if s == body),
            "matching hash must return Hit with the disk bytes"
        );
    }

    #[test]
    fn mismatch_when_hash_differs() {
        let path = scratch_path("mismatch");
        std::fs::write(&path, "current bytes\n").expect("write scratch");
        let outcome = read_disk_verified(path.to_str().expect("utf8 path"), true, Some(0x1234));
        let _ = std::fs::remove_file(&path);
        assert!(matches!(outcome, DiskReadOutcome::HashMismatch));
    }

    #[test]
    fn missing_when_file_absent() {
        let path = scratch_path("absent");
        let _ = std::fs::remove_file(&path); // ensure absent
        let outcome = read_disk_verified(path.to_str().expect("utf8 path"), true, Some(42));
        assert!(matches!(outcome, DiskReadOutcome::Missing));
    }

    #[test]
    fn not_recoverable_without_flag_or_hash() {
        let path = scratch_path("flag");
        std::fs::write(&path, "x").expect("write scratch");
        // recoverable=false → NotRecoverable regardless of hash
        assert!(matches!(
            read_disk_verified(path.to_str().expect("utf8 path"), false, Some(1)),
            DiskReadOutcome::NotRecoverable
        ));
        // hash absent → NotRecoverable regardless of flag
        assert!(matches!(
            read_disk_verified(path.to_str().expect("utf8 path"), true, None),
            DiskReadOutcome::NotRecoverable
        ));
        let _ = std::fs::remove_file(&path);
    }
}
