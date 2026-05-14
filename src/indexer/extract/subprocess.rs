use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use wait_timeout::ChildExt;

use super::ExtractError;

/// Outcome of a bounded subprocess invocation.
#[derive(Debug)]
pub(super) struct CapturedOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
}

/// Run a child process with a wall-clock timeout and an output-size cap.
///
/// - `cmd` is consumed; the caller is responsible for `arg()`/`env()` setup.
/// - On timeout, the child is killed and `ExtractError::Timeout` is returned.
/// - When stdout exceeds `max_bytes`, the child is killed; `truncated = true`;
///   the captured prefix is returned successfully.
/// - On non-zero exit, `ExtractError::Process { tool, status, stderr }` is returned.
/// - When `max_rss_bytes = Some(n)`, the child runs under `setrlimit(RLIMIT_AS, n)`
///   applied in `pre_exec`. If the child exceeds this cap, it dies from a
///   signal (commonly SIGSEGV/SIGABRT on glibc malloc failure, or SIGKILL
///   from an outer OOM); we surface that as `ExtractError::SubprocessKilled`.
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

    if let Some(rss_limit) = max_rss_bytes {
        // SAFETY: `pre_exec` runs in the forked child between fork and exec.
        // `setrlimit` is async-signal-safe and operates on the child's own
        // resource limits, with no shared mutable state observable to the
        // parent. Failure here means the child exits before exec runs; the
        // parent will observe that as a Process error with status -1.
        unsafe {
            cmd.pre_exec(move || {
                let rlim = libc::rlimit {
                    rlim_cur: rss_limit,
                    rlim_max: rss_limit,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &rlim) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let mut child = cmd.spawn().map_err(|e| ExtractError::Process {
        tool,
        status: -1,
        stderr: e.to_string(),
    })?;

    let mut stdout = child.stdout.take().expect("piped stdout configured above");
    let stderr_handle = child.stderr.take().expect("piped stderr configured above");

    let mut buf: Vec<u8> = Vec::with_capacity(max_bytes.min(64 * 1024));
    let mut chunk = [0u8; 16 * 1024];
    let deadline = Instant::now() + timeout;
    let mut truncated = false;

    loop {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ExtractError::Timeout);
        }
        match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let remaining = max_bytes.saturating_sub(buf.len());
                if remaining == 0 {
                    truncated = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                let copy = n.min(remaining);
                buf.extend_from_slice(&chunk[..copy]);
                if copy < n {
                    truncated = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ExtractError::Process {
                    tool,
                    status: -1,
                    stderr: e.to_string(),
                });
            }
        }
    }

    let remaining = deadline.saturating_duration_since(Instant::now());
    let status = if remaining.is_zero() {
        match child.try_wait() {
            Ok(Some(s)) => s,
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ExtractError::Timeout);
            }
        }
    } else {
        match child.wait_timeout(remaining) {
            Ok(Some(s)) => s,
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ExtractError::Timeout);
            }
            Err(e) => {
                return Err(ExtractError::Process {
                    tool,
                    status: -1,
                    stderr: e.to_string(),
                });
            }
        }
    };

    let mut stderr_buf = Vec::new();
    let _ = std::io::Read::take(stderr_handle, 64 * 1024).read_to_end(&mut stderr_buf);

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
}
