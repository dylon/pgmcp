//! Filesystem-availability probe shared by the disk watchdog and the outbox.
//!
//! Wraps a single `statvfs(2)` call and exposes **both** axes of ENOSPC:
//! free bytes (`f_bavail · f_frsize`) **and** free inodes (`f_favail`). A
//! filesystem can refuse a write because it is out of *either* — a disk with
//! gigabytes free but no inodes left fails exactly like a full one — so the
//! watchdog must monitor both (per the 2026-06-11 incident follow-up).

use std::path::Path;

/// A point-in-time availability reading for one filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsAvail {
    /// Bytes available to an unprivileged process (`f_bavail · f_frsize`).
    pub avail_bytes: u64,
    /// Inodes available to an unprivileged process (`f_favail`). `u64::MAX`
    /// when the filesystem does not report inode counts (e.g. some network or
    /// in-memory filesystems report `0`/unknown — see note below).
    pub avail_inodes: u64,
}

impl FsAvail {
    /// Element-wise minimum — used to fold many watched filesystems into the
    /// single worst-case reading the watchdog acts on.
    pub fn min(self, other: FsAvail) -> FsAvail {
        FsAvail {
            avail_bytes: self.avail_bytes.min(other.avail_bytes),
            avail_inodes: self.avail_inodes.min(other.avail_inodes),
        }
    }
}

/// `statvfs(path)` → free bytes + free inodes, or `None` if the syscall fails
/// (path does not exist, permission denied, etc.).
///
/// Some filesystems (tmpfs without an inode limit, certain network mounts)
/// report `f_files == 0`, i.e. "inodes are not a bounded resource here". We map
/// that to `u64::MAX` so the inode floor never spuriously trips on such a mount.
pub fn fs_avail(path: &Path) -> Option<FsAvail> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut s) };
    if rc != 0 {
        return None;
    }
    let avail_bytes = (s.f_bavail as u64).saturating_mul(s.f_frsize as u64);
    // `f_files == 0` ⇒ the filesystem does not bound inodes; treat as unlimited.
    let avail_inodes = if s.f_files == 0 {
        u64::MAX
    } else {
        s.f_favail as u64
    };
    Some(FsAvail {
        avail_bytes,
        avail_inodes,
    })
}

/// Convenience: free bytes only (the watchdog/outbox use [`fs_avail`] for both
/// axes; this preserves the original `target_cleanup` call site).
pub fn avail_bytes(path: &Path) -> Option<u64> {
    fs_avail(path).map(|a| a.avail_bytes)
}

/// Total bytes of the filesystem containing `path` (`f_blocks · f_frsize`), or
/// `None` if the `statvfs` call fails. Used by the disk-pressure report to
/// compute used-percentage (a fuller signal than free-bytes on a large disk:
/// 95% full with 100+ GiB free still warrants attention).
pub fn fs_total(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut s) };
    if rc != 0 {
        return None;
    }
    Some((s.f_blocks as u64).saturating_mul(s.f_frsize as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_total_is_at_least_available() {
        // `/` always exists; total capacity must be ≥ the free portion.
        let total = fs_total(Path::new("/")).expect("statvfs / total");
        let avail = avail_bytes(Path::new("/")).expect("statvfs / avail");
        assert!(total >= avail, "total {total} < avail {avail}");
        assert!(total > 0, "root filesystem reports zero total bytes");
    }

    #[test]
    fn missing_path_reports_none() {
        assert!(fs_total(Path::new("/pgmcp/no/such/path/xyzzy")).is_none());
    }
}
