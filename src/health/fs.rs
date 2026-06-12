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
