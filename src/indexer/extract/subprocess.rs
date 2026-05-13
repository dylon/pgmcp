use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use wait_timeout::ChildExt;

use super::ExtractError;

/// Outcome of a bounded subprocess invocation.
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
pub(super) fn run_bounded(
    mut cmd: Command,
    tool: &'static str,
    timeout: Duration,
    max_bytes: usize,
) -> Result<CapturedOutput, ExtractError> {
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

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
        return Err(ExtractError::Process {
            tool,
            status: status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
        });
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
) -> Result<(String, bool), ExtractError> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    let captured = run_bounded(cmd, tool, timeout, max_bytes)?;
    let text = String::from_utf8_lossy(&captured.stdout).into_owned();
    Ok((text, captured.truncated))
}
