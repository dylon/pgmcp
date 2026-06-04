//! Phase-2B **client-agnostic** file-event capture via an in-kernel BPF probe.
//!
//! Phase 2A (the Claude Code `PostToolUse` hook) is precise but covers only
//! Claude Code. This path attributes file touches for *any* connected client
//! (Codex, an editor's language server, a shell) by tracing the `openat`/`open`
//! syscalls of exactly the PIDs pgmcp already tracks in `mcp_clients`, and
//! recording each as an `ebpf`-source [`client_file_events`] row. The live
//! client PID set (maintained by Phase 1 + the liveness cron) is the elegant
//! in-kernel filter: we trace the clients we know, nothing else.
//!
//! ## Why bpftrace, not an in-tree `aya-ebpf` loader
//!
//! pgmcp deliberately has **no cargo features** (CUDA is mandatory; swap seams
//! are traits), and `scripts/verify.sh` builds the whole workspace on **stable**
//! Rust. A self-contained kernel-side `aya-ebpf` program needs nightly, a custom
//! `bpfel-unknown-none` target, and `-Z build-std` — none of which can be gated
//! behind a *runtime* `[clients] ebpf_enabled` flag, so vendoring it in-tree
//! would force nightly + a BPF cross-build into every `verify.sh` run and break
//! the contract. The project's hosts ship `bpftrace`/`bcc` (an in-kernel BPF VM
//! reached from userspace), so this module drives a `bpftrace` probe over a
//! pipe: it compiles on stable, adds zero build-time dependency, and stays
//! **off by default** (`ebpf_enabled = false`) so cap-less hosts are unaffected.
//! The probe needs `CAP_BPF`+`CAP_PERFMON` (or root) at *run* time; absent that,
//! the child exits and we log the reason and back off — never spin.
//!
//! The wire format between the probe and this consumer is one this module
//! *defines* (`E\t<pid>\t<flags>\t<path>` via a controlled `printf`), so parsing
//! is a defined protocol, not fragile screen-scraping.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use sqlx::PgPool;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::proc_clients::file_events::FileOp;

/// Linux `O_ACCMODE` mask — the low two bits of `open` flags hold the access
/// mode (`O_RDONLY=0`, `O_WRONLY=1`, `O_RDWR=2`).
const O_ACCMODE: i64 = 0o3;

/// A parsed `openat`/`open` trace event emitted by the probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEvent {
    pub pid: i32,
    pub flags: i64,
    pub path: String,
}

/// Classify an `open` `flags` value into a [`FileOp`]: any writable access mode
/// (`O_WRONLY`/`O_RDWR`) is a [`FileOp::Write`]; a read-only open is
/// [`FileOp::Open`]. We cannot see the eventual read/write *payload* from the
/// open alone, and an open-for-write is the high-signal edit event — Claude
/// Code, Codex, and editors all `open(…, O_WRONLY|O_CREAT|O_TRUNC)` to save.
pub fn classify_op(flags: i64) -> FileOp {
    if flags & O_ACCMODE != 0 {
        FileOp::Write
    } else {
        FileOp::Open
    }
}

/// Parse one probe output line of the form `E\t<pid>\t<flags>\t<path>`. Returns
/// `None` for bpftrace's attach banner and any malformed / non-absolute row.
pub fn parse_trace_line(line: &str) -> Option<RawEvent> {
    let rest = line.strip_prefix("E\t")?;
    let mut f = rest.splitn(3, '\t');
    let pid = f.next()?.trim().parse::<i32>().ok()?;
    let flags = f.next()?.trim().parse::<i64>().ok()?;
    let path = f.next()?;
    // Absolute paths only — drops relative opens (resolved against an unknown
    // cwd) and bpftrace's `(unknown)`/empty string sentinels.
    if !path.starts_with('/') {
        return None;
    }
    Some(RawEvent {
        pid,
        flags,
        path: path.to_string(),
    })
}

/// Build the bpftrace program tracing `openat`+`open` for exactly `pids`,
/// emitting `E\t<pid>\t<flags>\t<path>`. Both tracepoints expose `args->flags`
/// (a `long`) and `args->filename` (a userspace `const char *`) directly, so the
/// probe is BTF-independent and robust across kernels. Caller guarantees `pids`
/// is non-empty (an empty predicate is not a valid filter).
fn build_program(pids: &HashSet<i32>) -> String {
    let pred = pids
        .iter()
        .map(|p| format!("pid == {p}"))
        .collect::<Vec<_>>()
        .join(" || ");
    format!(
        "tracepoint:syscalls:sys_enter_openat /{pred}/ \
            {{ printf(\"E\\t%d\\t%d\\t%s\\n\", pid, args->flags, str(args->filename)); }}\n\
         tracepoint:syscalls:sys_enter_open /{pred}/ \
            {{ printf(\"E\\t%d\\t%d\\t%s\\n\", pid, args->flags, str(args->filename)); }}"
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

/// Live, PID-resolved MCP clients: the in-kernel trace filter set plus a
/// `pid → mcp_session_id` map so captured events join back to the client row.
async fn fetch_live_pids(
    pool: &PgPool,
) -> Result<(HashSet<i32>, HashMap<i32, String>), sqlx::Error> {
    let rows: Vec<(i32, String)> = sqlx::query_as(
        "SELECT pid, mcp_session_id FROM mcp_clients WHERE alive AND pid IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    let mut pids = HashSet::with_capacity(rows.len());
    let mut map = HashMap::with_capacity(rows.len());
    for (pid, sid) in rows {
        pids.insert(pid);
        map.insert(pid, sid);
    }
    Ok((pids, map))
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
    /// The live client PID set changed — respawn with the new filter.
    PidSetChanged,
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

/// Handle one parsed event: workspace-root prefilter → TTL dedup → resolve
/// project/file/session → insert an `ebpf` `client_file_events` row.
#[allow(clippy::too_many_arguments)]
async fn handle_event(
    pool: &PgPool,
    pid_session: &HashMap<i32, String>,
    roots: &[String],
    dedup_secs: u64,
    dedup: &mut HashMap<String, Instant>,
    ev: RawEvent,
) {
    // Cheap string prefilter so a busy client's opens of /usr, /etc, libs, and
    // its own transcript never reach the database.
    if !roots.iter().any(|r| ev.path.starts_with(r.as_str())) {
        return;
    }
    let op = classify_op(ev.flags);
    let key = format!("{}:{}:{}", ev.pid, op.as_str(), ev.path);
    let now = Instant::now();
    if let Some(prev) = dedup.get(&key)
        && now.duration_since(*prev) < Duration::from_secs(dedup_secs.max(1))
    {
        return; // same (pid, op, path) within the dedup window — collapse
    }
    dedup.insert(key, now);
    if dedup.len() > 4096 {
        let window = Duration::from_secs(dedup_secs.max(1));
        dedup.retain(|_, t| now.duration_since(*t) < window);
    }

    // Longest-prefix project match (NULL ⇒ not under any indexed project: skip).
    let project_id = match crate::db::queries::find_project_by_cwd(pool, &ev.path).await {
        Ok(Some(p)) => p.id,
        Ok(None) => return,
        Err(e) => {
            debug!(error = %e, "ebpf: project resolve failed");
            return;
        }
    };
    let file_id: Option<i64> = sqlx::query_scalar("SELECT id FROM indexed_files WHERE path = $1")
        .bind(&ev.path)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
    let session = pid_session.get(&ev.pid).cloned();
    if let Err(e) = sqlx::query(
        "INSERT INTO client_file_events
            (mcp_session_id, pid, file_id, project_id, abs_path, op, source, ts)
         VALUES ($1, $2, $3, $4, $5, $6, 'ebpf', now())",
    )
    .bind(session)
    .bind(ev.pid)
    .bind(file_id)
    .bind(project_id)
    .bind(&ev.path)
    .bind(op.as_str())
    .execute(pool)
    .await
    {
        debug!(error = %e, "ebpf: client_file_events insert failed");
    }
}

/// Run one bpftrace invocation for the current PID set, streaming events until
/// the set changes, the child dies, or shutdown fires.
#[allow(clippy::too_many_arguments)]
async fn run_one_probe(
    tracer: &PathBuf,
    pids: &HashSet<i32>,
    pid_session: &HashMap<i32, String>,
    pool: &PgPool,
    roots: &[String],
    refresh_secs: u64,
    dedup_secs: u64,
    dedup: &mut HashMap<String, Instant>,
    shutdown: &CancellationToken,
) -> ProbeExit {
    let program = build_program(pids);
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
                if let Ok((now_pids, _)) = fetch_live_pids(pool).await
                    && &now_pids != pids
                {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return ProbeExit::PidSetChanged;
                }
            }
            line = lines.next_line() => {
                match line {
                    Ok(Some(l)) => {
                        if let Some(ev) = parse_trace_line(&l) {
                            handle_event(pool, pid_session, roots, dedup_secs, dedup, ev).await;
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
/// `JoinHandle`; the task loops — (re)fetching the live client PID set, spawning
/// a `bpftrace` probe scoped to it, and respawning when the set changes — until
/// `shutdown` fires. `roots` are the workspace path prefixes (`[workspace]
/// paths`) used to prefilter events before any DB hit.
pub fn start_ebpf_consumer(
    pool: PgPool,
    roots: Vec<String>,
    refresh_secs: u64,
    dedup_secs: u64,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Some(tracer) = locate_tracer() else {
            warn!(
                "[clients] ebpf_enabled = true but `bpftrace` was not found on PATH; \
                 Phase-2B eBPF capture is disabled (install bpftrace, or grant the daemon \
                 CAP_BPF+CAP_PERFMON, to enable client-agnostic file attribution)"
            );
            return;
        };
        // Normalise roots to trimmed, non-empty prefixes once.
        let roots: Vec<String> = roots
            .into_iter()
            .map(|r| r.trim_end_matches('/').to_string())
            .filter(|r| !r.is_empty())
            .collect();
        info!(
            tracer = %tracer.display(),
            refresh_secs, dedup_secs,
            roots = roots.len(),
            "eBPF file-event capture started (Phase 2B, source='ebpf')"
        );

        let mut dedup: HashMap<String, Instant> = HashMap::new();
        loop {
            if shutdown.is_cancelled() {
                return;
            }
            let (pids, pid_session) = match fetch_live_pids(&pool).await {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "ebpf: live-pid fetch failed");
                    if sleep_or_cancel(refresh_secs, &shutdown).await {
                        return;
                    }
                    continue;
                }
            };
            if pids.is_empty() {
                // No PID-resolved clients yet — wait for one to connect.
                if sleep_or_cancel(refresh_secs, &shutdown).await {
                    return;
                }
                continue;
            }
            match run_one_probe(
                &tracer,
                &pids,
                &pid_session,
                &pool,
                &roots,
                refresh_secs,
                dedup_secs,
                &mut dedup,
                &shutdown,
            )
            .await
            {
                ProbeExit::Shutdown => return,
                ProbeExit::PidSetChanged => continue,
                ProbeExit::Failed(reason) => {
                    // Most often a permission/attach failure (no CAP_BPF). Back
                    // off generously so a cap-less host does not log-spam; the
                    // loop self-heals if caps are later granted.
                    warn!(reason = %reason, "ebpf: probe stopped; backing off 60s");
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
        // O_RDONLY = 0 → Open; O_WRONLY = 1, O_RDWR = 2 → Write (with/without
        // O_CREAT(0o100)/O_TRUNC(0o1000) high bits set).
        assert_eq!(classify_op(0), FileOp::Open);
        assert_eq!(classify_op(0o1), FileOp::Write);
        assert_eq!(classify_op(0o2), FileOp::Write);
        assert_eq!(classify_op(0o100 | 0o1), FileOp::Write); // O_CREAT|O_WRONLY
        assert_eq!(classify_op(0o1101 & !0o3), FileOp::Open); // mode bits cleared
    }

    #[test]
    fn parse_trace_line_accepts_well_formed_events() {
        let ev = parse_trace_line("E\t12345\t577\t/home/u/ws/proj/src/main.rs")
            .expect("well-formed event parses");
        assert_eq!(ev.pid, 12345);
        assert_eq!(ev.flags, 577); // 0o1101 = O_WRONLY|O_CREAT|O_TRUNC
        assert_eq!(ev.path, "/home/u/ws/proj/src/main.rs");
        assert_eq!(classify_op(ev.flags), FileOp::Write);
    }

    #[test]
    fn parse_trace_line_rejects_banner_and_malformed() {
        // bpftrace attach banner and stray lines.
        assert_eq!(parse_trace_line("Attaching 2 probes..."), None);
        assert_eq!(parse_trace_line(""), None);
        // Non-E-prefixed.
        assert_eq!(parse_trace_line("X\t1\t0\t/x"), None);
        // Relative / sentinel path is dropped (unknown cwd).
        assert_eq!(parse_trace_line("E\t1\t0\trelative/path"), None);
        assert_eq!(parse_trace_line("E\t1\t0\t(unknown)"), None);
        // Non-numeric pid / flags.
        assert_eq!(parse_trace_line("E\tnotapid\t0\t/x"), None);
        assert_eq!(parse_trace_line("E\t1\tnotflags\t/x"), None);
    }

    #[test]
    fn parse_trace_line_keeps_paths_with_spaces_and_tabs() {
        // splitn(3) means only the first two tabs split fields — a path may
        // itself contain spaces (and even tabs) and survives intact.
        let ev = parse_trace_line("E\t9\t0\t/home/u/My Docs/a.txt").expect("space path");
        assert_eq!(ev.path, "/home/u/My Docs/a.txt");
        let ev2 = parse_trace_line("E\t9\t0\t/home/u/od\tap.txt").expect("tab-in-path");
        assert_eq!(ev2.path, "/home/u/od\tap.txt");
    }

    #[test]
    fn build_program_filters_to_the_pid_set() {
        let pids: HashSet<i32> = [101, 202].into_iter().collect();
        let prog = build_program(&pids);
        assert!(prog.contains("sys_enter_openat"));
        assert!(prog.contains("sys_enter_open "), "legacy open traced too");
        assert!(prog.contains("pid == 101"));
        assert!(prog.contains("pid == 202"));
        assert!(prog.contains("||"), "multi-pid predicate is disjunctive");
        assert!(prog.contains("str(args->filename)"));
    }

    #[test]
    fn build_program_single_pid_has_no_disjunction() {
        let pids: HashSet<i32> = [7].into_iter().collect();
        let prog = build_program(&pids);
        assert!(prog.contains("pid == 7"));
        assert!(!prog.contains("||"));
    }
}
