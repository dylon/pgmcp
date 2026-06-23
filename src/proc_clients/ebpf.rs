//! eBPF file-event capture (Phase 2B/2C) — client-agnostic syscall tracing that
//! attributes `open`/`openat` to a connected client **and, in cgroup mode, every
//! subprocess it spawns**.
//!
//! ## Two modes (ADR-022)
//!
//! - [`EbpfMode::Pid`] (Phase 2B, `source='ebpf'`) — trace exactly the live
//!   client PIDs in `mcp_clients`. Sees the agent process itself, *not* its
//!   children, so a `cargo build` it launches is invisible.
//! - [`EbpfMode::Cgroup`] (Phase 2C, `source='ebpf_cgroup'`) — trace by
//!   `bpf_get_current_cgroup_id()`. cgroup membership is **inherited across
//!   `fork`/`exec`**, so a single `cgroup == <id>` predicate captures the agent
//!   *and* its whole process subtree (`cargo` → `rustc` → `rg`, …). Clients
//!   without a private registered cgroup (interactive / shared scope) fall back
//!   to a `pid == <p>` term in the same mixed predicate, so they still get
//!   per-PID capture rather than nothing.
//!
//! Either way the probe is a thin **producer**: it parses each trace line and
//! emits a [`FileTouchEvent`] into the shared reactive ingestion stream
//! (`proc_clients::ingest`) via [`StatsTracker::emit_file_event`]. Dedup,
//! batching, project/file resolution, and the DB insert all live downstream in
//! the stream — this module no longer touches the database at all.
//!
//! ## Why bpftrace, not an in-tree `aya-ebpf` loader
//!
//! pgmcp builds the whole workspace on **stable** Rust with no cargo features
//! (`scripts/verify.sh`). A self-contained kernel-side `aya-ebpf` program needs
//! nightly, a `bpfel-unknown-none` target, and `-Z build-std`, none of which can
//! hide behind a *runtime* flag, so it would force a BPF cross-build into every
//! `verify.sh` run. Driving the host's `bpftrace` over a pipe compiles on stable,
//! adds zero build-time dependency, and stays off by default. The probe needs
//! `CAP_BPF`+`CAP_PERFMON` (or root) at *run* time; absent that the child exits,
//! we log the reason and back off — never spin.
//!
//! The wire format is one this module *defines* — `E\t<pid>\t<cgroupid>\t<flags>\t<path>`
//! via a controlled `printf` — so parsing is a defined protocol, not screen-scraping.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::proc_clients::file_events::{FileEventSource, FileOp, FileTouchEvent};
use crate::stats::tracker::StatsTracker;

/// Linux `O_ACCMODE` mask — the low two bits of `open` flags hold the access
/// mode (`O_RDONLY=0`, `O_WRONLY=1`, `O_RDWR=2`).
const O_ACCMODE: i64 = 0o3;

/// Which filter the probe applies, and therefore which `source` it records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EbpfMode {
    /// Per-PID (Phase 2B): trace exactly the client PIDs. `source='ebpf'`.
    Pid,
    /// Per-cgroup (Phase 2C): trace the client's whole process subtree.
    /// `source='ebpf_cgroup'`.
    Cgroup,
}

impl EbpfMode {
    fn source(self) -> FileEventSource {
        match self {
            EbpfMode::Pid => FileEventSource::Ebpf,
            EbpfMode::Cgroup => FileEventSource::EbpfCgroup,
        }
    }
}

/// A parsed `openat`/`open` trace event emitted by the probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEvent {
    pub pid: i32,
    pub cgroup_id: u64,
    pub flags: i64,
    pub path: String,
}

/// The live filter set + a `(cgroup|pid) → mcp_session_id` map for attribution.
/// In cgroup mode a client with a known `cgroup_id` contributes a `cgroup` term;
/// one without (interactive / shared scope) falls back to a `pid` term. In pid
/// mode every client contributes a `pid` term.
#[derive(Debug, Default)]
struct LiveTargets {
    cgroups: HashSet<u64>,
    pids: HashSet<i32>,
    cgroup_session: HashMap<u64, String>,
    pid_session: HashMap<i32, String>,
}

impl LiveTargets {
    fn is_empty(&self) -> bool {
        self.cgroups.is_empty() && self.pids.is_empty()
    }
    /// Only the *filter sets* determine the probe predicate; the session maps are
    /// userspace-only attribution and can change without a respawn.
    fn same_filter(&self, other: &LiveTargets) -> bool {
        self.cgroups == other.cgroups && self.pids == other.pids
    }
}

/// Classify an `open` `flags` value into a [`FileOp`]: any writable access mode
/// (`O_WRONLY`/`O_RDWR`) is a [`FileOp::Write`]; a read-only open is
/// [`FileOp::Open`]. An open-for-write is the high-signal edit event — Claude
/// Code, Codex, `rustc`, and editors all `open(…, O_WRONLY|O_CREAT|O_TRUNC)`.
pub fn classify_op(flags: i64) -> FileOp {
    if flags & O_ACCMODE != 0 {
        FileOp::Write
    } else {
        FileOp::Open
    }
}

/// Parse one probe line of the form `E\t<pid>\t<cgroupid>\t<flags>\t<path>`.
/// Returns `None` for bpftrace's attach banner and any malformed / non-absolute
/// row. `splitn(4, '\t')` means a path containing tabs survives intact.
pub fn parse_trace_line(line: &str) -> Option<RawEvent> {
    let rest = line.strip_prefix("E\t")?;
    let mut f = rest.splitn(4, '\t');
    let pid = f.next()?.trim().parse::<i32>().ok()?;
    let cgroup_id = f.next()?.trim().parse::<u64>().ok()?;
    let flags = f.next()?.trim().parse::<i64>().ok()?;
    let path = f.next()?;
    // Absolute paths only — drops relative opens (resolved against an unknown
    // cwd) and bpftrace's `(unknown)`/empty-string sentinels.
    if !path.starts_with('/') {
        return None;
    }
    Some(RawEvent {
        pid,
        cgroup_id,
        flags,
        path: path.to_string(),
    })
}

/// Build the bpftrace program for `targets`, emitting
/// `E\t<pid>\t<cgroupid>\t<flags>\t<path>`. The predicate ORs every `cgroup == X`
/// and `pid == Y` term; when `capture_reads` is false an in-kernel write-only
/// guard (`& O_ACCMODE != 0`) drops read-only opens so a build's tens of
/// thousands of header reads never cross into userspace. Caller guarantees
/// `targets` is non-empty (an empty predicate is not a valid filter).
fn build_program(targets: &LiveTargets, capture_reads: bool) -> String {
    let mut terms: Vec<String> = Vec::with_capacity(targets.cgroups.len() + targets.pids.len());
    for c in &targets.cgroups {
        terms.push(format!("cgroup == {c}"));
    }
    for p in &targets.pids {
        terms.push(format!("pid == {p}"));
    }
    let pred = terms.join(" || ");
    let guard = if capture_reads {
        String::new()
    } else {
        " && (args->flags & 3) != 0".to_string()
    };
    format!(
        "tracepoint:syscalls:sys_enter_openat /({pred}){guard}/ \
            {{ printf(\"E\\t%d\\t%lu\\t%d\\t%s\\n\", pid, cgroup, args->flags, str(args->filename)); }}\n\
         tracepoint:syscalls:sys_enter_open /({pred}){guard}/ \
            {{ printf(\"E\\t%d\\t%lu\\t%d\\t%s\\n\", pid, cgroup, args->flags, str(args->filename)); }}"
    )
}

/// Locate the `bpftrace` binary on `PATH` or in the usual install locations.
fn locate_tracer() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("bpftrace");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    [
        "/usr/bin/bpftrace",
        "/usr/local/bin/bpftrace",
        "/bin/bpftrace",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
}

/// Read the live targets for `mode` from `mcp_clients`. In cgroup mode a client
/// with a `cgroup_id` becomes a cgroup target (its whole subtree); one without
/// falls back to a pid target. In pid mode every client is a pid target.
async fn fetch_live_targets(pool: &PgPool, mode: EbpfMode) -> Result<LiveTargets, sqlx::Error> {
    let rows: Vec<(i32, Option<i64>, String)> = sqlx::query_as(
        "SELECT pid, cgroup_id, mcp_session_id FROM mcp_clients WHERE alive AND pid IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    let mut t = LiveTargets::default();
    for (pid, cgroup_id, sid) in rows {
        match (mode, cgroup_id) {
            (EbpfMode::Cgroup, Some(cg)) => {
                let cg = cg as u64; // BIGINT i64 → kernel u64 (bit-cast inverse).
                t.cgroups.insert(cg);
                t.cgroup_session.insert(cg, sid);
            }
            _ => {
                t.pids.insert(pid);
                t.pid_session.insert(pid, sid);
            }
        }
    }
    Ok(t)
}

/// Sleep up to `secs`, returning `true` if shutdown fired first.
async fn sleep_or_cancel(secs: u64, shutdown: &CancellationToken) -> bool {
    tokio::select! {
        _ = shutdown.cancelled() => true,
        _ = tokio::time::sleep(Duration::from_secs(secs.max(1))) => false,
    }
}

/// Why a single probe invocation stopped streaming.
enum ProbeExit {
    /// The daemon is shutting down.
    Shutdown,
    /// The live target set changed — respawn with the new filter.
    TargetsChanged,
    /// The probe exited / failed; the string is the captured reason.
    Failed(String),
}

/// Read the child's stderr (the failure reason) and reap it.
async fn drain_stderr(child: &mut Child) -> String {
    let mut buf = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut buf).await;
    }
    let _ = child.wait().await;
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        "bpftrace exited (stdout closed)".to_string()
    } else {
        trimmed.lines().take(3).collect::<Vec<_>>().join("; ")
    }
}

/// Handle one parsed event: workspace-root prefilter → resolve the owning session
/// (cgroup first, then pid) → emit a [`FileTouchEvent`] into the ingestion
/// stream. Synchronous and best-effort; dedup/batch/insert happen downstream.
fn handle_event(
    stats: &StatsTracker,
    roots: &[String],
    mode: EbpfMode,
    targets: &LiveTargets,
    ev: RawEvent,
) {
    // Cheap string prefilter so a busy subtree's opens of /usr, /etc, libs, and
    // ~/.cargo never reach the stream — only `[workspace] paths` survive.
    if !roots.iter().any(|r| ev.path.starts_with(r.as_str())) {
        return;
    }
    // Recover the owning client's session: by cgroup (the subtree's root agent),
    // else by pid (the fallback/legacy term). NULL ⇒ the cgroup-attribution
    // join in `client_project_matrix` still resolves it at query time.
    let mcp_session_id = targets
        .cgroup_session
        .get(&ev.cgroup_id)
        .or_else(|| targets.pid_session.get(&ev.pid))
        .cloned();
    stats.emit_file_event(FileTouchEvent {
        source: mode.source(),
        op: classify_op(ev.flags),
        abs_path: ev.path,
        pid: Some(ev.pid),
        ppid: None,
        root_pid: None,
        cgroup_id: Some(ev.cgroup_id),
        mcp_session_id,
        session_id: None,
        agent_id: None,
    });
}

/// Run one bpftrace invocation for the current targets, streaming events until
/// the target set changes, the child dies, or shutdown fires.
#[allow(clippy::too_many_arguments)]
async fn run_one_probe(
    tracer: &PathBuf,
    targets: &LiveTargets,
    pool: &PgPool,
    roots: &[String],
    mode: EbpfMode,
    capture_reads: bool,
    refresh_secs: u64,
    stats: &StatsTracker,
    shutdown: &CancellationToken,
) -> ProbeExit {
    let program = build_program(targets, capture_reads);
    let mut child = match Command::new(tracer)
        .arg("-B")
        .arg("line") // line-buffered stdout → timely per-event reads
        .arg("-e")
        .arg(&program)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return ProbeExit::Failed(format!("spawn bpftrace: {e}")),
    };
    let Some(stdout) = child.stdout.take() else {
        return ProbeExit::Failed("bpftrace stdout not piped".to_string());
    };
    let mut lines = BufReader::new(stdout).lines();
    let mut tick = tokio::time::interval(Duration::from_secs(refresh_secs.max(1)));
    tick.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                let _ = child.start_kill();
                return ProbeExit::Shutdown;
            }
            _ = tick.tick() => {
                if let Ok(now) = fetch_live_targets(pool, mode).await
                    && !now.same_filter(targets)
                {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return ProbeExit::TargetsChanged;
                }
            }
            line = lines.next_line() => {
                match line {
                    Ok(Some(l)) => {
                        if let Some(ev) = parse_trace_line(&l) {
                            handle_event(stats, roots, mode, targets, ev);
                        }
                    }
                    Ok(None) => return ProbeExit::Failed(drain_stderr(&mut child).await),
                    Err(e) => {
                        let _ = child.start_kill();
                        return ProbeExit::Failed(format!("read probe stdout: {e}"));
                    }
                }
            }
        }
    }
}

/// Spawn the long-lived eBPF file-event consumer. Returns immediately with a
/// `JoinHandle`; the task loops — (re)fetching the live target set, spawning a
/// `bpftrace` probe scoped to it, and respawning when the set changes — until
/// `shutdown` fires. `roots` are the `[workspace] paths` prefixes used to
/// prefilter events. `stats` carries the ingestion-stream sender events are
/// emitted into.
#[allow(clippy::too_many_arguments)]
pub fn start_ebpf_consumer(
    pool: PgPool,
    roots: Vec<String>,
    refresh_secs: u64,
    capture_reads: bool,
    mode: EbpfMode,
    stats: Arc<StatsTracker>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Some(tracer) = locate_tracer() else {
            warn!(
                "[clients] eBPF capture enabled but `bpftrace` was not found on PATH; \
                 disabled (install bpftrace, or grant CAP_BPF+CAP_PERFMON, to enable \
                 client-agnostic file attribution)"
            );
            return;
        };
        let roots: Vec<String> = roots
            .into_iter()
            .map(|r| r.trim_end_matches('/').to_string())
            .filter(|r| !r.is_empty())
            .collect();
        info!(
            tracer = %tracer.display(),
            refresh_secs, capture_reads,
            mode = ?mode,
            roots = roots.len(),
            "eBPF file-event capture started"
        );

        loop {
            if shutdown.is_cancelled() {
                return;
            }
            let targets = match fetch_live_targets(&pool, mode).await {
                Ok(v) => v,
                Err(e) => {
                    error!(error = %e, "ebpf: live-target fetch failed");
                    if sleep_or_cancel(refresh_secs, &shutdown).await {
                        return;
                    }
                    continue;
                }
            };
            if targets.is_empty() {
                // No PID-resolved clients yet — wait for one to connect.
                if sleep_or_cancel(refresh_secs, &shutdown).await {
                    return;
                }
                continue;
            }
            match run_one_probe(
                &tracer,
                &targets,
                &pool,
                &roots,
                mode,
                capture_reads,
                refresh_secs,
                &stats,
                &shutdown,
            )
            .await
            {
                ProbeExit::Shutdown => return,
                ProbeExit::TargetsChanged => continue,
                ProbeExit::Failed(reason) => {
                    // Most often a permission/attach failure (no CAP_BPF). Back
                    // off generously so a cap-less host does not log-spam; the
                    // loop self-heals if caps are later granted.
                    error!(reason = %reason, "ebpf: probe stopped; backing off 60s");
                    if sleep_or_cancel(refresh_secs.max(60), &shutdown).await {
                        return;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_op_reads_vs_writes() {
        assert_eq!(classify_op(0), FileOp::Open);
        assert_eq!(classify_op(0o1), FileOp::Write);
        assert_eq!(classify_op(0o2), FileOp::Write);
        assert_eq!(classify_op(0o100 | 0o1), FileOp::Write); // O_CREAT|O_WRONLY
        assert_eq!(classify_op(0o1101 & !0o3), FileOp::Open); // mode bits cleared
    }

    #[test]
    fn parse_trace_line_accepts_well_formed_events() {
        let ev = parse_trace_line("E\t12345\t303106\t577\t/home/u/ws/proj/src/main.rs")
            .expect("well-formed event parses");
        assert_eq!(ev.pid, 12345);
        assert_eq!(ev.cgroup_id, 303106);
        assert_eq!(ev.flags, 577); // 0o1101 = O_WRONLY|O_CREAT|O_TRUNC
        assert_eq!(ev.path, "/home/u/ws/proj/src/main.rs");
        assert_eq!(classify_op(ev.flags), FileOp::Write);
    }

    #[test]
    fn parse_trace_line_rejects_banner_and_malformed() {
        assert_eq!(parse_trace_line("Attaching 2 probes..."), None);
        assert_eq!(parse_trace_line(""), None);
        assert_eq!(parse_trace_line("X\t1\t2\t0\t/x"), None);
        // Relative / sentinel path is dropped (unknown cwd).
        assert_eq!(parse_trace_line("E\t1\t2\t0\trelative/path"), None);
        assert_eq!(parse_trace_line("E\t1\t2\t0\t(unknown)"), None);
        // Non-numeric pid / cgroup / flags.
        assert_eq!(parse_trace_line("E\tnotapid\t2\t0\t/x"), None);
        assert_eq!(parse_trace_line("E\t1\tnotacg\t0\t/x"), None);
        assert_eq!(parse_trace_line("E\t1\t2\tnotflags\t/x"), None);
    }

    #[test]
    fn parse_trace_line_keeps_paths_with_spaces_and_tabs() {
        // splitn(4) means only the first three tabs split fields — a path may
        // itself contain spaces (and even tabs) and survives intact.
        let ev = parse_trace_line("E\t9\t5\t0\t/home/u/My Docs/a.txt").expect("space path");
        assert_eq!(ev.path, "/home/u/My Docs/a.txt");
        let ev2 = parse_trace_line("E\t9\t5\t0\t/home/u/od\tap.txt").expect("tab-in-path");
        assert_eq!(ev2.path, "/home/u/od\tap.txt");
    }

    #[test]
    fn build_program_cgroup_and_pid_terms() {
        let mut t = LiveTargets::default();
        t.cgroups.insert(303106);
        t.pids.insert(101);
        let prog = build_program(&t, true);
        assert!(prog.contains("sys_enter_openat"));
        assert!(prog.contains("sys_enter_open "), "legacy open traced too");
        assert!(prog.contains("cgroup == 303106"));
        assert!(prog.contains("pid == 101"));
        assert!(prog.contains("||"), "multi-target predicate is disjunctive");
        assert!(prog.contains("%lu"), "cgroup id printed as u64");
        assert!(prog.contains("str(args->filename)"));
        // capture_reads = true ⇒ no write-only guard.
        assert!(!prog.contains("& 3) != 0"));
    }

    #[test]
    fn build_program_write_only_guard_when_reads_disabled() {
        let mut t = LiveTargets::default();
        t.cgroups.insert(7);
        let prog = build_program(&t, false);
        assert!(prog.contains("cgroup == 7"));
        assert!(prog.contains("& 3) != 0"), "write-only guard injected");
    }

    #[test]
    fn live_targets_same_filter_ignores_session_maps() {
        let mut a = LiveTargets::default();
        a.cgroups.insert(1);
        a.cgroup_session.insert(1, "sess-a".to_string());
        let mut b = LiveTargets::default();
        b.cgroups.insert(1);
        b.cgroup_session.insert(1, "sess-b".to_string()); // different session
        // Same filter set ⇒ no respawn needed despite the session map differing.
        assert!(a.same_filter(&b));
        b.pids.insert(99);
        assert!(!a.same_filter(&b)); // filter set changed ⇒ respawn
    }
}
