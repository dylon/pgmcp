use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crossbeam_channel::{RecvTimeoutError, bounded};

use super::ExtractError;

/// Outcome of a bounded subprocess invocation.
#[derive(Debug)]
pub(super) struct CapturedOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
}

/// Stderr ring-buffer cap. Past this, the reader keeps draining the pipe
/// (to prevent the child from blocking on a full pipe) but discards the
/// extra bytes.
const STDERR_CAP: usize = 64 * 1024;

/// Grace window after `SIGTERM` before escalating to `SIGKILL` on a
/// timeout / size-cap kill. Pandoc + Tesseract typically clean up well
/// under a tenth of a second; 500ms is generous without delaying the
/// outer cron tick.
const KILL_GRACE: Duration = Duration::from_millis(500);

/// Run a child process with a wall-clock timeout and an output-size cap.
///
/// Robustness guarantees vs. the prior implementation:
///
/// - **Process group**: `setsid` runs in `pre_exec` so the child becomes
///   the leader of a new process group. Timeouts and size-cap kills send
///   `SIGTERM` (then `SIGKILL` after `KILL_GRACE`) to the *group* via
///   `killpg`, stopping orphaned grandchildren (e.g., `pandoc → pdflatex`)
///   instead of leaving them reparented to PID 1.
/// - **Concurrent stderr drain**: a dedicated reader thread keeps draining
///   `stderr` while the main thread reads `stdout`. A child that writes
///   more than `STDERR_CAP` bytes to stderr can no longer deadlock the
///   pipeline.
/// - **Deadline-aware wait**: stdout reads happen via a crossbeam channel
///   filled by a reader thread, so the main thread can call
///   `recv_timeout(remaining)` and wake exactly on the deadline. A child
///   that pauses but doesn't exit no longer blocks indefinitely on a
///   `read()`.
///
/// Errors:
/// - `ExtractError::Timeout` — wall-clock deadline expired.
/// - `ExtractError::Process { tool, status, stderr }` — clean non-zero exit.
/// - `ExtractError::SubprocessKilled { tool, signal }` — died from a signal
///   (rlimit, OOM, abort).
pub(super) fn run_bounded(
    mut cmd: Command,
    tool: &'static str,
    timeout: Duration,
    max_bytes: usize,
    max_rss_bytes: Option<u64>,
) -> Result<CapturedOutput, ExtractError> {
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    // SAFETY: `pre_exec` runs in the forked child between fork and exec.
    // `setsid` and `setrlimit` are both async-signal-safe and touch only
    // the child's own state. Failure here makes the child exit before
    // exec; the parent observes that as `Process { status: -1 }`.
    unsafe {
        cmd.pre_exec(move || {
            // Become the leader of a new process group/session so the
            // parent can kill the whole tree via `killpg` and orphaned
            // grandchildren don't outlive the timeout.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if let Some(rss_limit) = max_rss_bytes {
                let rlim = libc::rlimit {
                    rlim_cur: rss_limit,
                    rlim_max: rss_limit,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &rlim) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn().map_err(|e| ExtractError::Process {
        tool,
        status: -1,
        stderr: e.to_string(),
    })?;

    // `setsid` made the child a process-group leader with PGID == PID.
    let pgid = child.id() as i32;
    let stdout = child.stdout.take().expect("piped stdout configured above");
    let stderr = child.stderr.take().expect("piped stderr configured above");

    let (stdout_tx, stdout_rx) = bounded::<std::io::Result<Vec<u8>>>(2);
    let stdout_thread = std::thread::Builder::new()
        .name(format!("pgmcp-extract-stdout-{tool}"))
        .spawn(move || {
            let mut reader = stdout;
            let mut chunk = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if stdout_tx.send(Ok(chunk[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = stdout_tx.send(Err(e));
                        break;
                    }
                }
            }
        })
        .expect("spawn stdout reader thread");

    let stderr_thread = std::thread::Builder::new()
        .name(format!("pgmcp-extract-stderr-{tool}"))
        .spawn(move || -> Vec<u8> {
            let mut reader = stderr;
            let mut accumulated: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let room = STDERR_CAP.saturating_sub(accumulated.len());
                        if room > 0 {
                            let take = n.min(room);
                            accumulated.extend_from_slice(&chunk[..take]);
                        }
                        // Beyond STDERR_CAP we still drain the pipe so the
                        // child cannot block on a full stderr; the extra
                        // bytes are dropped.
                    }
                }
            }
            accumulated
        })
        .expect("spawn stderr reader thread");

    let mut buf: Vec<u8> = Vec::with_capacity(max_bytes.min(64 * 1024));
    let deadline = Instant::now() + timeout;
    let mut truncated = false;
    let mut io_error: Option<std::io::Error> = None;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            kill_pg_graceful(&mut child, pgid);
            join_readers(stdout_thread, stderr_thread);
            return Err(ExtractError::Timeout);
        }
        match stdout_rx.recv_timeout(remaining) {
            Ok(Ok(bytes)) => {
                let remaining_buf = max_bytes.saturating_sub(buf.len());
                if remaining_buf == 0 {
                    truncated = true;
                    kill_pg_graceful(&mut child, pgid);
                    break;
                }
                let copy = bytes.len().min(remaining_buf);
                buf.extend_from_slice(&bytes[..copy]);
                if copy < bytes.len() {
                    truncated = true;
                    kill_pg_graceful(&mut child, pgid);
                    break;
                }
            }
            Ok(Err(e)) => {
                io_error = Some(e);
                kill_pg_graceful(&mut child, pgid);
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                kill_pg_graceful(&mut child, pgid);
                join_readers(stdout_thread, stderr_thread);
                return Err(ExtractError::Timeout);
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    // Drain any remaining queued stdout chunks the reader produced before
    // the child closed its end. Non-blocking — `try_recv` returns
    // immediately once the channel is empty or disconnected.
    if !truncated && io_error.is_none() {
        while let Ok(msg) = stdout_rx.try_recv() {
            match msg {
                Ok(bytes) => {
                    let remaining_buf = max_bytes.saturating_sub(buf.len());
                    if remaining_buf == 0 {
                        truncated = true;
                        kill_pg_graceful(&mut child, pgid);
                        break;
                    }
                    let copy = bytes.len().min(remaining_buf);
                    buf.extend_from_slice(&bytes[..copy]);
                    if copy < bytes.len() {
                        truncated = true;
                        kill_pg_graceful(&mut child, pgid);
                        break;
                    }
                }
                Err(e) => {
                    io_error = Some(e);
                    kill_pg_graceful(&mut child, pgid);
                    break;
                }
            }
        }
    }

    // The child has either exited on its own (pipes closed) or been
    // signalled. Either way `wait` should return promptly because the
    // process is on its way out.
    let status = child.wait().map_err(|e| ExtractError::Process {
        tool,
        status: -1,
        stderr: e.to_string(),
    })?;

    let stderr_buf = stderr_thread.join().unwrap_or_default();
    let _ = stdout_thread.join();

    if let Some(e) = io_error {
        return Err(ExtractError::Process {
            tool,
            status: -1,
            stderr: e.to_string(),
        });
    }

    if !status.success() && !truncated {
        // `code()` returns None when the child died from a signal.
        // SubprocessKilled is the canonical OOM/rlimit-hit/abort case;
        // Process is for clean non-zero exits.
        return match status.code() {
            Some(code) => Err(ExtractError::Process {
                tool,
                status: code,
                stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
            }),
            None => Err(ExtractError::SubprocessKilled {
                tool,
                signal: status.signal().unwrap_or(0),
            }),
        };
    }

    Ok(CapturedOutput {
        stdout: buf,
        stderr: stderr_buf,
        truncated,
    })
}

/// Send SIGTERM to the entire process group; if the child hasn't exited
/// after `KILL_GRACE`, escalate to SIGKILL. Best-effort — errors are
/// swallowed (the child may have already exited).
fn kill_pg_graceful(child: &mut Child, pgid: i32) {
    // SAFETY: `killpg` is async-signal-safe and operates by PGID with no
    // shared mutable state. We always created the group via `setsid` in
    // pre_exec, so the PGID equals the child's PID.
    unsafe {
        libc::killpg(pgid, libc::SIGTERM);
    }
    let start = Instant::now();
    while start.elapsed() < KILL_GRACE {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break,
        }
    }
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

/// Best-effort cleanup of the reader threads. The threads exit naturally
/// once the child closes its pipes (`read` returns 0 / errno = EBADF).
fn join_readers(
    stdout_thread: std::thread::JoinHandle<()>,
    stderr_thread: std::thread::JoinHandle<Vec<u8>>,
) {
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
}

/// Convenience: invoke a CLI tool with the given args, returning the
/// captured stdout decoded as UTF-8 (lossy on bad bytes).
pub(super) fn run_tool_utf8(
    tool: &'static str,
    bin: &Path,
    args: &[&std::ffi::OsStr],
    timeout: Duration,
    max_bytes: usize,
    max_rss_bytes: Option<u64>,
) -> Result<(String, bool), ExtractError> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    let captured = run_bounded(cmd, tool, timeout, max_bytes, max_rss_bytes)?;
    let text = String::from_utf8_lossy(&captured.stdout).into_owned();
    Ok((text, captured.truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_bounded_detects_signal_kill_as_subprocess_killed() {
        // `sh -c 'kill -9 $$'` makes the shell kill itself with SIGKILL.
        // The wait returns with no exit code and signal=9; the new arm in
        // `run_bounded` maps that to `ExtractError::SubprocessKilled`.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("kill -9 $$");
        let result = run_bounded(cmd, "sh", Duration::from_secs(5), 1024, None);
        match result {
            Err(ExtractError::SubprocessKilled { tool, signal }) => {
                assert_eq!(tool, "sh");
                assert_eq!(signal, 9, "expected SIGKILL (9), got {signal}");
            }
            other => panic!("expected SubprocessKilled, got {other:?}"),
        }
    }

    #[test]
    fn run_bounded_applies_rlimit_as_to_child() {
        // `sh -c 'ulimit -v'` prints the address-space limit the child
        // inherits in KiB. With `max_rss_bytes = Some(64 MiB)` the child
        // should report 65536. This proves `pre_exec(setrlimit)` actually
        // applies to the spawned process.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("ulimit -v");
        let result = run_bounded(
            cmd,
            "sh",
            Duration::from_secs(5),
            1024,
            Some(64 * 1024 * 1024), // 64 MiB
        );
        match result {
            Ok(captured) => {
                let out = String::from_utf8_lossy(&captured.stdout);
                let limit_kb: u64 = out
                    .trim()
                    .parse()
                    .unwrap_or_else(|_| panic!("non-numeric ulimit output: {out:?}"));
                assert_eq!(
                    limit_kb, 65536,
                    "expected 64 MiB rlimit (65536 KiB) inherited by child"
                );
            }
            other => panic!("expected captured output, got {other:?}"),
        }
    }

    #[test]
    fn run_bounded_no_rlimit_when_none() {
        // When `max_rss_bytes = None`, no `setrlimit` call is made and
        // the child inherits the parent's RLIMIT_AS (usually unlimited,
        // i.e. `ulimit -v` reports "unlimited" not a number).
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("ulimit -v");
        let result = run_bounded(cmd, "sh", Duration::from_secs(5), 1024, None);
        let captured = result.expect("sh ulimit -v should not fail");
        let out = String::from_utf8_lossy(&captured.stdout);
        let trimmed = out.trim();
        // Most CI/dev hosts run with RLIMIT_AS unlimited; if a test host
        // has set a parent limit, accept anything other than 65536 (the
        // value the rlimit-applied test pins).
        assert!(
            trimmed == "unlimited" || trimmed.parse::<u64>().is_ok_and(|n| n != 65536),
            "unexpected inherited ulimit -v: {trimmed:?}"
        );
    }

    #[test]
    fn run_bounded_kills_process_group_on_timeout() {
        // The shell backgrounds a long sleep, writes its PID to a temp
        // file, then waits forever. After `run_bounded` times out and
        // calls `killpg` on the shell's pgid, the backgrounded sleep
        // must also die — it belongs to the same process group (it
        // didn't call setsid of its own).
        let tmp = std::env::temp_dir().join(format!(
            "pgmcp-subprocess-pgtest-{}.pid",
            std::process::id()
        ));
        let tmp_arg = tmp.to_string_lossy().into_owned();
        let script = format!("sleep 60 & echo $! > '{tmp_arg}'; wait");
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(&script);
        let start = Instant::now();
        let result = run_bounded(cmd, "sh", Duration::from_millis(500), 1024, None);
        let elapsed = start.elapsed();
        assert!(
            matches!(result, Err(ExtractError::Timeout)),
            "expected Timeout, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "timeout exceeded {elapsed:?}"
        );

        // Give the kernel a beat to reap the killed sleep.
        std::thread::sleep(Duration::from_millis(200));
        let pid_text = std::fs::read_to_string(&tmp).expect("temp file must have been written");
        let _ = std::fs::remove_file(&tmp);
        let sleep_pid: i32 = pid_text.trim().parse().expect("sleep PID is numeric");
        // `kill(pid, 0)` returns -1 / ESRCH when the process is gone.
        // SAFETY: the syscall is async-signal-safe and only inspects
        // process state; signal 0 never delivers anything.
        let alive = unsafe { libc::kill(sleep_pid, 0) } == 0;
        assert!(
            !alive,
            "backgrounded sleep {sleep_pid} survived killpg of the process group"
        );
    }

    #[test]
    fn run_bounded_drains_large_stderr() {
        // The child writes >STDERR_CAP bytes to stderr, then prints to
        // stdout, then exits. With the old code this deadlocked on the
        // full stderr pipe; with the concurrent stderr drain it succeeds.
        //
        // `yes a` produces "a\n" forever on stdout. The pipe sends that
        // to `head -c N`, which reads N bytes and writes them to its own
        // stdout. The `>&2` redirects head's stdout to stderr; head then
        // closes its stdin and yes dies on SIGPIPE. Finally `printf
        // hello` writes a deterministic stdout payload we can match on.
        let payload_bytes = STDERR_CAP * 2;
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(format!("yes a | head -c {payload_bytes} >&2; printf hello"));
        let result = run_bounded(cmd, "sh", Duration::from_secs(5), 1024, None);
        let captured = result.expect("stdout=hello must succeed despite large stderr");
        assert_eq!(captured.stdout, b"hello");
        assert!(
            captured.stderr.len() <= STDERR_CAP,
            "stderr buffer exceeded cap: {}",
            captured.stderr.len()
        );
    }

    #[test]
    fn run_bounded_respects_deadline_on_blocked_child() {
        // The child sleeps without writing anything; the old blocking
        // read would never wake. The new `recv_timeout(deadline)` wakes
        // on the deadline regardless.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("sleep 30");
        let start = Instant::now();
        let result = run_bounded(cmd, "sh", Duration::from_millis(500), 1024, None);
        let elapsed = start.elapsed();
        assert!(
            matches!(result, Err(ExtractError::Timeout)),
            "expected Timeout, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "deadline overshoot: {elapsed:?}"
        );
    }
}
