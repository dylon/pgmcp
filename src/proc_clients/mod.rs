//! Linux `/proc`-based resolution of an MCP client's OS identity: PID (from a
//! TCP peer address), working directory, liveness, process-incarnation
//! fingerprint, and currently-open files.
//!
//! The daemon serves MCP over streamable HTTP on loopback. A connected client's
//! source `(ip, port)` — captured via `ConnectInfo` (see
//! [`crate::mcp::server::extract_peer_addr`]) — is mapped back to the owning PID
//! by finding the `/proc/net/tcp{,6}` row whose *local* endpoint is the client
//! and whose *remote* endpoint is the daemon (on loopback both ends appear in
//! our tables), reading its socket inode, then scanning `/proc/<pid>/fd/*` for
//! the process that holds `socket:[inode]`. From the PID we then derive:
//!
//! - cwd via `readlink /proc/<pid>/cwd`,
//! - liveness via `kill(pid, 0)` (the idiom from
//!   `crate::indexer::extract::subprocess`),
//! - a start-time fingerprint (field 22 of `/proc/<pid>/stat`) that guards
//!   against PID reuse, and
//! - the set of currently-open regular files (a best-effort Phase-2 supplement).
//!
//! Every function is best-effort and returns `None`/empty off-Linux or on a
//! permission/parse/race failure — the daemon runs as the same user as its
//! clients, so same-user `/proc` reads succeed in the common case.

pub mod ebpf;
pub mod file_events;

use std::net::SocketAddr;
use std::path::PathBuf;

/// `kill(pid, 0) == 0` ⇒ the process exists and is signalable. A dead/reaped PID
/// yields `ESRCH` (→ `false`); `EPERM` means the process exists but is owned by
/// another user (→ `true`, it is alive). Linux-only; `false` elsewhere.
pub fn pid_alive(pid: i32) -> bool {
    #[cfg(target_os = "linux")]
    {
        if pid <= 0 {
            return false;
        }
        // SAFETY: signal 0 performs only existence/permission error-checking; it
        // never delivers a signal. Async-signal-safe, inspects process state only.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

/// Working directory of `pid`, via `readlink /proc/<pid>/cwd` (a magic symlink
/// whose target is the process's resolved cwd). `None` if the process is gone or
/// the link is unreadable.
pub fn read_process_cwd(pid: i32) -> Option<PathBuf> {
    if pid <= 0 {
        return None;
    }
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// Process start time in clock ticks since boot — field 22 (`starttime`) of
/// `/proc/<pid>/stat`. Together with the PID this fingerprints a specific
/// process incarnation, so a recycled PID (same number, different process) is
/// detectable as a mismatch. `None` if unreadable/unparsable.
pub fn proc_start_ticks(pid: i32) -> Option<u64> {
    if pid <= 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 2 (`comm`) is parenthesized and may itself contain spaces and `)`,
    // so split on the LAST ')'. The remainder begins at field 3 (`state`); thus
    // field 22 (`starttime`) is index 19 of the whitespace-split remainder.
    let rparen = stat.rfind(')')?;
    let rest = stat.get(rparen + 2..)?;
    rest.split_whitespace().nth(19)?.parse::<u64>().ok()
}

/// Resolve the OS PID that owns the *client* side of a loopback TCP connection
/// to this daemon. `peer` is the client's `(ip, port)` as seen by the server
/// (the accepted connection's *remote* address); `server_port` is the daemon's
/// listen port. On loopback both endpoints appear in `/proc/net/tcp{,6}`; the
/// client's row has `local_port == peer.port()` and `rem_port == server_port`
/// (the server's row has them reversed). We read that row's socket inode and
/// scan `/proc/<pid>/fd/*` for the holder. `None` on no match (non-loopback,
/// race, or permission). Linux-only.
pub fn resolve_pid_for_peer(peer: SocketAddr, server_port: u16) -> Option<i32> {
    let inode = socket_inode_for_peer(peer, server_port)?;
    pid_owning_socket_inode(inode)
}

/// Currently-open regular files held by `pid`, via `/proc/<pid>/fd/*` readlink,
/// filtered to absolute on-disk files (drops sockets/pipes/anon-inodes and the
/// `/dev`,`/proc`,`/sys` pseudo-filesystems). Best-effort; empty on error. This
/// is the Phase-2 `proc_fd` supplement only — Claude Code's open→write→close
/// means a poll rarely catches the file actually being edited.
pub fn list_open_files(pid: i32) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if pid <= 0 {
        return out;
    }
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return out;
    };
    for fd in fds.flatten() {
        if let Ok(target) = std::fs::read_link(fd.path())
            && target.is_absolute()
            && !target.starts_with("/dev")
            && !target.starts_with("/proc")
            && !target.starts_with("/sys")
            && target.is_file()
        {
            out.push(target);
        }
    }
    out
}

// ── internals ───────────────────────────────────────────────────────────────

/// Find the socket inode for the client's loopback socket by scanning both the
/// IPv4 and IPv6 connection tables.
fn socket_inode_for_peer(peer: SocketAddr, server_port: u16) -> Option<u64> {
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Some(inode) = scan_proc_net_tcp(path, peer.port(), server_port) {
            return Some(inode);
        }
    }
    None
}

/// Scan a `/proc/net/tcp{,6}`-format file for the row whose local/remote ports
/// match, returning its socket inode. Field layout (whitespace-separated):
/// `sl(0) local_address(1) rem_address(2) st(3) tx:rx(4) tr:tm(5) retrnsmt(6)
/// uid(7) timeout(8) inode(9) …`. Addresses are `HEXADDR:HEXPORT`; ports are
/// matched (address byte-order is avoided — the client source port is unique
/// among connections to our server, and pinning `rem_port == server_port`
/// disambiguates from the server's mirror row).
fn scan_proc_net_tcp(path: &str, want_local_port: u16, want_rem_port: u16) -> Option<u64> {
    let data = std::fs::read_to_string(path).ok()?;
    for line in data.lines().skip(1) {
        let mut f = line.split_whitespace();
        let (Some(local), Some(rem)) = (f.nth(1), f.next()) else {
            continue;
        };
        // After consuming local(1) and rem(2), the next item is st(3); nth(6)
        // skips st..timeout (idx 3..8) and yields inode (idx 9).
        let Some(inode_field) = f.nth(6) else {
            continue;
        };
        let (Some(lp), Some(rp)) = (hex_port(local), hex_port(rem)) else {
            continue;
        };
        if lp == want_local_port
            && rp == want_rem_port
            && let Ok(inode) = inode_field.parse::<u64>()
        {
            return Some(inode);
        }
    }
    None
}

/// Parse the `:HEXPORT` tail of a `/proc/net/tcp` address field into a `u16`.
fn hex_port(addr_field: &str) -> Option<u16> {
    let (_addr, port) = addr_field.split_once(':')?;
    u16::from_str_radix(port, 16).ok()
}

/// Scan `/proc/<pid>/fd/*` across all numeric PID dirs for the process holding
/// the given `socket:[inode]`. O(total open fds); runs once per session and is
/// cached by the caller.
fn pid_owning_socket_inode(inode: u64) -> Option<i32> {
    let needle = format!("socket:[{inode}]");
    let procfs = std::fs::read_dir("/proc").ok()?;
    for entry in procfs.flatten() {
        let fname = entry.file_name();
        let Some(name) = fname.to_str() else {
            continue;
        };
        let Ok(pid) = name.parse::<i32>() else {
            continue; // skip non-PID entries (cpuinfo, net, self, …)
        };
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue; // process gone or not ours
        };
        for fd in fds.flatten() {
            if let Ok(target) = std::fs::read_link(fd.path())
                && target.to_string_lossy() == needle
            {
                return Some(pid);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_alive_self_true_bogus_false() {
        let self_pid = std::process::id() as i32;
        assert!(pid_alive(self_pid), "own pid must be alive");
        assert!(!pid_alive(0x7fff_ffff), "absurd pid must be dead");
        assert!(!pid_alive(0));
        assert!(!pid_alive(-1));
    }

    #[test]
    fn read_process_cwd_self_matches_current_dir() {
        let self_pid = std::process::id() as i32;
        let via_proc = read_process_cwd(self_pid).expect("own /proc cwd readable");
        let via_std = std::env::current_dir().expect("current_dir");
        assert_eq!(via_proc.canonicalize().ok(), via_std.canonicalize().ok());
    }

    #[test]
    fn proc_start_ticks_self_some_bogus_none() {
        let self_pid = std::process::id() as i32;
        assert!(proc_start_ticks(self_pid).is_some());
        assert_eq!(proc_start_ticks(0x7fff_ffff), None);
    }

    #[test]
    fn hex_port_parses_and_rejects() {
        assert_eq!(hex_port("0100007F:0CEA"), Some(0x0CEA));
        assert_eq!(hex_port("00000000:0000"), Some(0));
        assert_eq!(hex_port("0100007F:0C1C"), Some(3100));
        assert_eq!(hex_port("no-colon"), None);
    }

    #[test]
    fn scan_proc_net_tcp_picks_client_row_not_server_mirror() {
        // Two loopback rows for one connection: the client socket
        // (local 0x9001 → rem 0x0C1C=3100, inode 424242) and the server's
        // mirror (local 0x0C1C → rem 0x9001, inode 999999). Matching
        // local==0x9001 && rem==3100 must select the client row's inode.
        let body = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:9001 0100007F:0C1C 01 00000000:00000000 00:00000000 00000000  1000        0 424242 1 ffff 100\n\
   1: 0100007F:0C1C 0100007F:9001 01 00000000:00000000 00:00000000 00000000  1000        0 999999 1 ffff 100\n";
        let tmp = std::env::temp_dir().join(format!("pgmcp_net_tcp_{}.txt", std::process::id()));
        std::fs::write(&tmp, body).expect("write synthetic /proc/net/tcp");
        let inode = scan_proc_net_tcp(tmp.to_str().expect("utf8 tmp path"), 0x9001, 3100);
        std::fs::remove_file(&tmp).ok();
        assert_eq!(inode, Some(424242));
    }

    #[test]
    fn scan_proc_net_tcp_no_match_is_none() {
        let body = "\
  sl  local_address rem_address   st …\n\
   0: 0100007F:1234 0100007F:5678 01 0 0 0 0 0 11111 1\n";
        let tmp =
            std::env::temp_dir().join(format!("pgmcp_net_tcp_nomatch_{}.txt", std::process::id()));
        std::fs::write(&tmp, body).expect("write synthetic");
        let inode = scan_proc_net_tcp(tmp.to_str().unwrap(), 0x9001, 3100);
        std::fs::remove_file(&tmp).ok();
        assert_eq!(inode, None);
    }
}
