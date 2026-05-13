use std::ffi::OsStr;
use std::path::Path;

use super::{
    ExtractError, ExtractOptions, Extracted, normalize::normalize_extracted_text, resolve_tool,
    subprocess::run_tool_utf8,
};

/// Extract PDF text via `pdftotext` (poppler-utils).
///
/// Flags chosen for accuracy + reading-order fidelity on multi-column papers:
///
/// * `-layout` — preserve physical layout (multi-column reading order).
/// * `-enc UTF-8` — force UTF-8 output regardless of locale.
/// * `-q` — suppress startup/banner noise on stderr.
/// * `-nopgbrk` — drop form-feed page-break characters; the normalization
///   pass strips them anyway, but `-nopgbrk` keeps the captured stream
///   cleaner up-front.
///
/// Output is normalized (NFKC, dehyphenation, page-number strip,
/// whitespace collapse) before being returned.
pub fn extract(path: &Path, opts: &ExtractOptions) -> Result<Option<Extracted>, ExtractError> {
    let bin = resolve_tool("pdftotext").ok_or(ExtractError::ToolMissing { tool: "pdftotext" })?;
    let source_size_bytes = std::fs::metadata(path).map_err(ExtractError::Io)?.len();

    let path_os = path.as_os_str();
    let args: [&OsStr; 7] = [
        OsStr::new("-layout"),
        OsStr::new("-enc"),
        OsStr::new("UTF-8"),
        OsStr::new("-q"),
        OsStr::new("-nopgbrk"),
        path_os,
        OsStr::new("-"),
    ];

    let (raw, truncated) = run_tool_utf8(
        "pdftotext",
        &bin,
        &args,
        opts.timeout,
        opts.max_extracted_bytes,
    )?;
    let text = normalize_extracted_text(&raw);
    Ok(Some(Extracted {
        text,
        truncated,
        source_size_bytes,
    }))
}
