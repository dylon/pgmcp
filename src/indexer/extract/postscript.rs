use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

use super::{
    ExtractError, ExtractOptions, Extracted, normalize::normalize_extracted_text, resolve_tool,
    subprocess::run_bounded,
};

/// Extract PostScript text via `ps2ascii` (ghostscript).
///
/// `ps2ascii` is a shell wrapper around `gs` with no useful flags;
/// `LC_ALL=C.UTF-8` is set so the wrapper emits UTF-8 regardless of the
/// daemon's locale. The captured stream is normalized before storage.
pub fn extract(path: &Path, opts: &ExtractOptions) -> Result<Option<Extracted>, ExtractError> {
    let bin = resolve_tool("ps2ascii").ok_or(ExtractError::ToolMissing { tool: "ps2ascii" })?;
    let source_size_bytes = std::fs::metadata(path).map_err(ExtractError::Io)?.len();

    let mut cmd = Command::new(&bin);
    cmd.arg::<&OsStr>(path.as_os_str());
    cmd.env("LC_ALL", "C.UTF-8");

    let captured = run_bounded(
        cmd,
        "ps2ascii",
        opts.timeout,
        opts.max_extracted_bytes,
        opts.max_subprocess_rss_bytes,
    )?;
    let raw = String::from_utf8_lossy(&captured.stdout);
    let text = normalize_extracted_text(&raw);
    Ok(Some(Extracted {
        text,
        truncated: captured.truncated,
        source_size_bytes,
    }))
}
