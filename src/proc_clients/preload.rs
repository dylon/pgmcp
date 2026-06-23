//! Unprivileged file-event capture via the `LD_PRELOAD` shim (ADR-022 Phase 2D).
//!
//! The companion C shim (`preload_shim.c`, preloaded into an agent's subtree by
//! `crucible/scripts/agent-scope.sh`) intercepts file-writing libc calls and
//! sends one `SOCK_DGRAM` datagram per event to this reader. We parse each
//! datagram and emit a [`FileTouchEvent`] into the shared reactive ingestion
//! stream — the same `Subject` the eBPF/hook/proc_fd sources feed
//! ([`crate::proc_clients::ingest`]) — so dedup, batching, project/file
//! resolution, and the DB insert are all reused.
//!
//! This path is **zero-privilege**: no caps, no root, and — unlike the eBPF
//! cgroup path — no requirement that the agent run in a private cgroup.
//! Attribution rests on `agent_id`, which the shim copies from `PGMCP_AGENT_ID`
//! (set by the wrapper) into every datagram; the `cgroup_id` is carried
//! opportunistically so the cgroup-join also works when an agent additionally
//! ran under a scope. It **complements** the eBPF path (the ingest `dedup_key`
//! collapses any overlap) and is blind to the same things any libc interposer is
//! (statically linked / Go-runtime / setuid children).
//!
//! Structurally this mirrors [`crate::proc_clients::ebpf::start_ebpf_consumer`]
//! (a long-lived `tokio` task driven by a `CancellationToken`) but is simpler: it
//! owns a bound `UnixDatagram` and loops on `recv` — no child process, no
//! live-target refetch, and no `PgPool` (the datagram is self-describing).

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UnixDatagram;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::proc_clients::file_events::{FileEventSource, FileOp, FileTouchEvent};
use crate::stats::tracker::StatsTracker;

/// A parsed datagram from the preload shim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreloadEvent {
    pub pid: i32,
    pub ppid: i32,
    /// cgroup-v2 id, or `0` when the shim couldn't read one (no scope).
    pub cgroup_id: u64,
    /// `PGMCP_AGENT_ID` from the wrapper; `None` when unset.
    pub agent_id: Option<String>,
    pub op: FileOp,
    pub flags: i64,
    pub path: String,
}

/// Parse one shim datagram: `P\t<pid>\t<ppid>\t<cgroupid>\t<agent_id>\t<op>\t<flags>\t<abs_path>`.
/// `splitn(7, '\t')` means the trailing path survives even if it contains tabs
/// (mirrors [`crate::proc_clients::ebpf::parse_trace_line`]). `None` for any
/// malformed / non-absolute / wrong-discriminator record.
pub fn parse_preload_line(bytes: &[u8]) -> Option<PreloadEvent> {
    let line = std::str::from_utf8(bytes).ok()?;
    // File-transport records are newline-terminated (the socket path is not); trim
    // so the trailing field (the path) never carries a stray '\n'/'\r'.
    let line = line.trim_end_matches(['\n', '\r']);
    let rest = line.strip_prefix("P\t")?;
    let mut f = rest.splitn(7, '\t');
    let pid = f.next()?.trim().parse::<i32>().ok()?;
    let ppid = f.next()?.trim().parse::<i32>().ok()?;
    let cgroup_id = f.next()?.trim().parse::<u64>().ok()?;
    let agent_id = {
        let a = f.next()?;
        if a.is_empty() {
            None
        } else {
            Some(a.to_string())
        }
    };
    let op = match f.next()? {
        "w" => FileOp::Write,
        "e" => FileOp::Edit,
        "r" => FileOp::Open,
        _ => return None,
    };
    let flags = f.next()?.trim().parse::<i64>().ok()?;
    let path = f.next()?;
    // Absolute paths only — the shim should always resolve, but be defensive.
    if !path.starts_with('/') {
        return None;
    }
    Some(PreloadEvent {
        pid,
        ppid,
        cgroup_id,
        agent_id,
        op,
        flags,
        path: path.to_string(),
    })
}

/// Workspace-root prefilter (identical to the eBPF path) → emit into the stream.
fn handle_event(stats: &StatsTracker, roots: &[String], capture_reads: bool, ev: PreloadEvent) {
    // Defense in depth: the shim only sends reads when PGMCP_FSTRACE_READS=1, but
    // honor the daemon-side `capture_reads` toggle too.
    if !capture_reads && ev.op == FileOp::Open {
        return;
    }
    if !roots.iter().any(|r| ev.path.starts_with(r.as_str())) {
        return;
    }
    stats.emit_file_event(FileTouchEvent {
        source: FileEventSource::Preload,
        op: ev.op,
        abs_path: ev.path,
        pid: Some(ev.pid),
        ppid: Some(ev.ppid),
        root_pid: None,
        cgroup_id: (ev.cgroup_id != 0).then_some(ev.cgroup_id),
        mcp_session_id: None,
        session_id: None,
        agent_id: ev.agent_id,
    });
}

/// Bind the datagram socket: create the parent dir (0700), unlink any stale node,
/// `bind`, then chmod 0600. `None` (disabled) on failure.
fn bind_socket(path: &Path) -> Option<UnixDatagram> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    // A stale socket from a previous daemon would otherwise EADDRINUSE.
    let _ = std::fs::remove_file(path);
    match UnixDatagram::bind(path) {
        Ok(sock) => {
            // Same-user only — nothing else on the host can send spoofed events.
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            Some(sock)
        }
        Err(e) => {
            warn!(socket = %path.display(), error = %e,
                  "preload: socket bind failed; preload capture disabled");
            None
        }
    }
}

/// Sleep up to `secs`, returning `true` if shutdown fired first.
async fn sleep_or_cancel(secs: u64, shutdown: &CancellationToken) -> bool {
    tokio::select! {
        _ = shutdown.cancelled() => true,
        _ = tokio::time::sleep(Duration::from_secs(secs.max(1))) => false,
    }
}

/// Spawn the long-lived preload file-event consumer. Binds `socket_path`, then
/// loops on `recv`, parsing each datagram and emitting into the ingestion stream
/// (`stats`), until `shutdown` fires. `roots` are the `[workspace] paths`
/// prefixes used to prefilter. Returns immediately with a `JoinHandle`.
pub fn start_preload_consumer(
    roots: Vec<String>,
    socket_path: PathBuf,
    capture_reads: bool,
    stats: Arc<StatsTracker>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let roots: Vec<String> = roots
            .into_iter()
            .map(|r| r.trim_end_matches('/').to_string())
            .filter(|r| !r.is_empty())
            .collect();

        loop {
            if shutdown.is_cancelled() {
                return;
            }
            // A bind failure is treated as persistent (perms / bad path): disable
            // until the next daemon restart, mirroring the eBPF path's
            // locate_tracer()==None → return.
            let Some(sock) = bind_socket(&socket_path) else {
                return;
            };
            info!(
                socket = %socket_path.display(),
                capture_reads,
                roots = roots.len(),
                "preload file-event capture started (Phase 2D, source='preload')"
            );

            // 64 KiB ≫ PATH_MAX + header; one datagram per recv (no framing).
            let mut buf = vec![0u8; 65536];
            let rebind = loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        let _ = std::fs::remove_file(&socket_path);
                        return;
                    }
                    r = sock.recv(&mut buf) => match r {
                        Ok(n) => {
                            if let Some(ev) = parse_preload_line(&buf[..n]) {
                                handle_event(&stats, &roots, capture_reads, ev);
                            }
                        }
                        Err(e) => {
                            error!(error = %e, "preload: recv failed; rebinding");
                            break true;
                        }
                    }
                }
            };
            let _ = std::fs::remove_file(&socket_path);
            if rebind && sleep_or_cancel(5, &shutdown).await {
                return;
            }
        }
    })
}

// ─── File transport (ADR-022 Phase 2E) ──────────────────────────────────────
// Codex's seccomp sandbox EPERMs socket I/O but allows `write()` to a file in a
// `writable_roots` dir, so its shim appends newline-terminated records to a
// per-launch `<agent_id>-<pid>.log` instead of a datagram socket. This tailer
// drains those files into the same ingest stream, and REAPS each file once its
// owning agent process exits (backstop to the wrapper's EXIT-trap `rm`) — so the
// trace files never accumulate.

/// Tail state for one trace file.
struct FileState {
    offset: u64,
    ino: u64,
    /// Bytes after the last `\n` — a record split across two reads; completed next drain.
    carry: Vec<u8>,
    /// Owning agent PID parsed from `<agent_id>-<pid>.log`, used by the reaper.
    /// `None` for a name without a numeric `-<pid>` suffix (never auto-reaped).
    pid: Option<i32>,
}

/// Extract the owning PID from an `<agent_id>-<pid>.log` filename (rsplit on the
/// last `-`, so `claude-code-1234.log` → 1234). `None` if the suffix isn't numeric.
fn parse_pid_from_name(path: &Path) -> Option<i32> {
    let stem = path.file_stem()?.to_str()?;
    let (_, pid) = stem.rsplit_once('-')?;
    pid.parse::<i32>().ok()
}

/// Drain newly-appended complete lines from one trace file → `handle_event`.
/// Handles inode-change (recreate), `size < offset` (truncation/rotation), a
/// partial trailing line (buffered in `carry`), and size-triggered in-place
/// rotation (`ftruncate(0)` once caught up — safe because the shim's `O_APPEND`
/// writers always re-seek to EOF).
fn drain_file(
    path: &Path,
    st: &mut FileState,
    roots: &[String],
    capture_reads: bool,
    rotate_bytes: u64,
    stats: &StatsTracker,
) {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return,
    };
    let ino = meta.ino();
    let size = meta.len();
    if st.ino == 0 {
        st.ino = ino;
    } else if ino != st.ino {
        // File replaced (recreate / tmpfs clear) — restart from the top.
        st.ino = ino;
        st.offset = 0;
        st.carry.clear();
    }
    if size < st.offset {
        // Truncated/rotated under us — restart from the top.
        st.offset = 0;
        st.carry.clear();
    }
    if size > st.offset {
        let want = (size - st.offset).min(1 << 20) as usize; // ≤1 MiB per drain
        let mut f = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return,
        };
        if f.seek(SeekFrom::Start(st.offset)).is_err() {
            return;
        }
        let mut chunk = vec![0u8; want];
        let nread = match f.read(&mut chunk) {
            Ok(n) => n,
            Err(_) => return,
        };
        st.offset += nread as u64;
        st.carry.extend_from_slice(&chunk[..nread]);
        let mut start = 0;
        while let Some(pos) = st.carry[start..].iter().position(|&b| b == b'\n') {
            if let Some(ev) = parse_preload_line(&st.carry[start..start + pos]) {
                handle_event(stats, roots, capture_reads, ev);
            }
            start += pos + 1;
        }
        st.carry.drain(0..start);
    }
    // In-place rotation once fully caught up + no partial pending.
    if st.carry.is_empty() && st.offset >= size && size >= rotate_bytes {
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
            let _ = f.set_len(0);
        }
        st.offset = 0;
    }
}

/// One sweep: drain every `*.log` in `dir`, then reap files whose owning agent
/// PID has exited and which are fully drained (the cleanup-on-exit backstop), and
/// drop state for files that already vanished (the wrapper trap got there first).
fn drain_dir(
    dir: &Path,
    states: &mut HashMap<PathBuf, FileState>,
    roots: &[String],
    capture_reads: bool,
    rotate_bytes: u64,
    stats: &StatsTracker,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut present: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("log") {
            continue;
        }
        present.insert(path.clone());
        let st = states.entry(path.clone()).or_insert_with(|| FileState {
            offset: 0,
            ino: 0,
            carry: Vec::new(),
            pid: parse_pid_from_name(&path),
        });
        drain_file(&path, st, roots, capture_reads, rotate_bytes, stats);
    }
    states.retain(|path, st| {
        if !present.contains(path) {
            return false; // already removed (wrapper EXIT trap) — just drop state
        }
        // Reap: owning agent exited AND we've consumed everything → delete the file.
        if let Some(pid) = st.pid
            && !crate::proc_clients::pid_alive(pid)
            && st.carry.is_empty()
        {
            let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            if st.offset >= size {
                let _ = std::fs::remove_file(path);
                return false;
            }
        }
        true
    });
}

/// Spawn the long-lived preload **file** consumer (ADR-022 Phase 2E): polls the
/// trace `dir` every 250 ms, draining the per-launch `<agent>-<pid>.log` files the
/// shim's file transport appends to (Codex, whose sandbox blocks the socket),
/// emitting into the same ingestion stream as the socket path. Reaps each file
/// when its owning agent exits, and removes all tracked files on shutdown.
pub fn start_preload_file_consumer(
    roots: Vec<String>,
    dir: PathBuf,
    capture_reads: bool,
    rotate_bytes: u64,
    stats: Arc<StatsTracker>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let roots: Vec<String> = roots
            .into_iter()
            .map(|r| r.trim_end_matches('/').to_string())
            .filter(|r| !r.is_empty())
            .collect();
        if std::fs::create_dir_all(&dir).is_err() {
            warn!(dir = %dir.display(), "preload-file: cannot create trace dir; capture disabled");
            return;
        }
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        info!(
            dir = %dir.display(),
            capture_reads, rotate_bytes,
            "preload FILE-event capture started (Phase 2E, source='preload')"
        );

        let mut states: HashMap<PathBuf, FileState> = HashMap::new();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        tick.tick().await; // consume the immediate first tick

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    // Remove every trace file we were tracking on clean shutdown.
                    for path in states.keys() {
                        let _ = std::fs::remove_file(path);
                    }
                    return;
                }
                _ = tick.tick() => {
                    drain_dir(&dir, &mut states, &roots, capture_reads, rotate_bytes, &stats);
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_well_formed_write() {
        let ev = parse_preload_line(
            b"P\t1234\t1000\t303106\tclaude-code\tw\t577\t/ws/proj/target/x.rlib",
        )
        .expect("well-formed datagram parses");
        assert_eq!(ev.pid, 1234);
        assert_eq!(ev.ppid, 1000);
        assert_eq!(ev.cgroup_id, 303106);
        assert_eq!(ev.agent_id.as_deref(), Some("claude-code"));
        assert_eq!(ev.op, FileOp::Write);
        assert_eq!(ev.flags, 577);
        assert_eq!(ev.path, "/ws/proj/target/x.rlib");
    }

    #[test]
    fn parse_empty_agent_and_no_cgroup() {
        let ev = parse_preload_line(b"P\t1\t2\t0\t\te\t0\t/ws/a").expect("parses");
        assert_eq!(ev.agent_id, None);
        assert_eq!(ev.cgroup_id, 0);
        assert_eq!(ev.op, FileOp::Edit);
    }

    #[test]
    fn parse_read_op() {
        let ev = parse_preload_line(b"P\t1\t2\t0\tcodex\tr\t0\t/ws/a").expect("parses");
        assert_eq!(ev.op, FileOp::Open);
        assert_eq!(ev.agent_id.as_deref(), Some("codex"));
    }

    #[test]
    fn parse_keeps_tab_in_path() {
        let ev = parse_preload_line(b"P\t1\t2\t0\ta\tw\t0\t/ws/a\tb.txt").expect("parses");
        assert_eq!(ev.path, "/ws/a\tb.txt");
    }

    #[test]
    fn parse_rejects_malformed() {
        assert_eq!(parse_preload_line(b""), None);
        assert_eq!(parse_preload_line(b"E\t1\t2\t0\ta\tw\t0\t/x"), None); // wrong discriminator
        assert_eq!(parse_preload_line(b"P\t1\t2\t0\ta\tz\t0\t/x"), None); // bad op char
        assert_eq!(parse_preload_line(b"P\tx\t2\t0\ta\tw\t0\t/x"), None); // bad pid
        assert_eq!(parse_preload_line(b"P\t1\t2\tx\ta\tw\t0\t/x"), None); // bad cgroup
        assert_eq!(parse_preload_line(b"P\t1\t2\t0\ta\tw\t0\trel/path"), None); // relative
    }

    #[test]
    fn parse_tolerates_trailing_newline() {
        let ev = parse_preload_line(b"P\t1\t2\t0\tcodex\tw\t577\t/ws/a.rs\n").expect("parses");
        assert_eq!(ev.path, "/ws/a.rs");
        let ev2 = parse_preload_line(b"P\t1\t2\t0\tcodex\tw\t577\t/ws/a.rs\r\n").expect("parses");
        assert_eq!(ev2.path, "/ws/a.rs");
    }

    #[test]
    fn parse_pid_from_name_extracts_suffix() {
        assert_eq!(parse_pid_from_name(Path::new("/x/codex-12345.log")), Some(12345));
        assert_eq!(parse_pid_from_name(Path::new("/x/claude-code-9.log")), Some(9));
        assert_eq!(parse_pid_from_name(Path::new("/x/codex.log")), None);
        assert_eq!(parse_pid_from_name(Path::new("/x/codex-abc.log")), None);
    }

    fn drain_stats() -> (StatsTracker, crossbeam_channel::Receiver<FileTouchEvent>) {
        let stats = StatsTracker::new();
        let (tx, rx) = crossbeam_channel::bounded(256);
        stats.set_file_event_sender(tx);
        (stats, rx)
    }

    #[test]
    fn drain_emits_lines_and_advances_offset() {
        let (stats, rx) = drain_stats();
        let tmp = std::env::temp_dir().join(format!("pgmcp-fst-drain-{}.log", std::process::id()));
        std::fs::write(
            &tmp,
            b"P\t1\t2\t0\tcodex\tw\t0\t/ws/a.rs\nP\t3\t4\t0\tcodex\te\t0\t/ws/b.rs\n",
        )
        .unwrap();
        let roots = vec!["/ws".to_string()];
        let mut st = FileState {
            offset: 0,
            ino: 0,
            carry: Vec::new(),
            pid: None,
        };
        drain_file(&tmp, &mut st, &roots, true, 8 << 20, &stats);
        let n = rx.try_iter().count();
        let len = std::fs::metadata(&tmp).unwrap().len();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(n, 2);
        assert!(st.carry.is_empty());
        assert_eq!(st.offset, len);
    }

    #[test]
    fn drain_buffers_partial_last_line() {
        use std::io::Write as _;
        let (stats, rx) = drain_stats();
        let tmp =
            std::env::temp_dir().join(format!("pgmcp-fst-partial-{}.log", std::process::id()));
        let roots = vec!["/ws".to_string()];
        let mut st = FileState {
            offset: 0,
            ino: 0,
            carry: Vec::new(),
            pid: None,
        };
        // Second record has no trailing '\n' — it must buffer, not emit torn.
        std::fs::write(&tmp, b"P\t1\t2\t0\tc\tw\t0\t/ws/a.rs\nP\t3\t4\t0\tc\tw\t0\t/ws/b").unwrap();
        drain_file(&tmp, &mut st, &roots, true, 8 << 20, &stats);
        assert_eq!(rx.try_iter().count(), 1); // only the complete a.rs
        assert!(!st.carry.is_empty());
        let mut f = std::fs::OpenOptions::new().append(true).open(&tmp).unwrap();
        f.write_all(b".rs\n").unwrap();
        drain_file(&tmp, &mut st, &roots, true, 8 << 20, &stats);
        let n = rx.try_iter().count();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(n, 1); // now the completed b.rs
        assert!(st.carry.is_empty());
    }

    #[test]
    fn drain_resets_on_truncation() {
        let (stats, rx) = drain_stats();
        let tmp = std::env::temp_dir().join(format!("pgmcp-fst-trunc-{}.log", std::process::id()));
        let roots = vec!["/ws".to_string()];
        let mut st = FileState {
            offset: 0,
            ino: 0,
            carry: Vec::new(),
            pid: None,
        };
        std::fs::write(
            &tmp,
            b"P\t1\t2\t0\tc\tw\t0\t/ws/aaaaa.rs\nP\t3\t4\t0\tc\tw\t0\t/ws/bbbbb.rs\n",
        )
        .unwrap();
        drain_file(&tmp, &mut st, &roots, true, 8 << 20, &stats);
        assert_eq!(rx.try_iter().count(), 2);
        // Overwrite with SHORTER content → size < offset → reset, re-read from 0.
        std::fs::write(&tmp, b"P\t9\t9\t0\tc\tw\t0\t/ws/c.rs\n").unwrap();
        drain_file(&tmp, &mut st, &roots, true, 8 << 20, &stats);
        let n = rx.try_iter().count();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(n, 1); // c.rs after the truncation reset
    }

    #[test]
    fn reaper_deletes_dead_pid_file_when_drained() {
        let (stats, _rx) = drain_stats();
        let dir = std::env::temp_dir().join(format!("pgmcp-fst-reap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 0x7fff_ffff is the absurd-PID sentinel the proc_clients tests trust as dead.
        let dead = 0x7fff_ffff_i32;
        let f = dir.join(format!("codex-{dead}.log"));
        std::fs::write(&f, b"P\t1\t2\t0\tcodex\tw\t0\t/ws/a.rs\n").unwrap();
        let roots = vec!["/ws".to_string()];
        let mut states = HashMap::new();
        // Drains the line, then (same sweep) reaps: dead owner + fully drained.
        drain_dir(&dir, &mut states, &roots, true, 8 << 20, &stats);
        let existed = f.exists();
        let tracked = states.contains_key(&f);
        std::fs::remove_dir_all(&dir).ok();
        assert!(!existed, "dead-pid trace file should be reaped after drain");
        assert!(!tracked, "reaped file's state should be dropped");
    }
}
