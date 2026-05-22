use std::ffi::OsStr;
use std::path::Path;

use super::{
    ExtractError, ExtractOptions, Extracted, normalize::normalize_extracted_text, resolve_tool,
    subprocess::run_tool_utf8,
};

/// Extract plain text from a binary office-format file.
///
/// `language` must be one of: `docx`, `doc`, `rtf`, `odt`, `epub`.
///
/// `doc` is special — the shipped pandoc (≤3.x) only supports DOCX, not
/// legacy Word DOC. Calling `pandoc --from doc` produces `Unknown input
/// format doc`. We try `catdoc` then `antiword` (both pure binary-DOC
/// readers) before falling back to pandoc, which preserves indexing of
/// legacy DOC files when the helpers are installed and surfaces a clean
/// `ToolMissing` error when none are. All other formats route directly
/// to pandoc.
pub fn extract_office(
    language: &str,
    path: &Path,
    opts: &ExtractOptions,
) -> Result<Option<Extracted>, ExtractError> {
    if language == "doc" {
        return extract_legacy_doc(path, opts);
    }
    let pandoc_format = match language {
        "docx" => "docx",
        "rtf" => "rtf",
        "odt" => "odt",
        "epub" => "epub",
        other => {
            return Err(ExtractError::Encoding(format!(
                "extract_office called with unsupported language `{}`",
                other
            )));
        }
    };
    extract_via_pandoc(pandoc_format, path, opts)
}

/// Read a legacy Word `.doc` file. Tries `catdoc` (preferred for
/// embedded charset handling), then `antiword`, then surfaces a
/// `ToolMissing { tool: "catdoc" }` error so the embed pool reports the
/// gap once and skips. `pandoc` is intentionally NOT in the fallback
/// chain because it does not support legacy DOC — calling it produces
/// 88+/day "Unknown input format doc" errors that masquerade as a
/// real extraction problem when in fact pandoc has nothing to offer.
fn extract_legacy_doc(
    path: &Path,
    opts: &ExtractOptions,
) -> Result<Option<Extracted>, ExtractError> {
    let source_size_bytes = std::fs::metadata(path).map_err(ExtractError::Io)?.len();
    let path_os = path.as_os_str();

    if let Some(bin) = resolve_tool("catdoc") {
        // catdoc reads .doc and writes plain text to stdout.
        let args: [&OsStr; 1] = [path_os];
        let (raw, truncated) = run_tool_utf8(
            "catdoc",
            &bin,
            &args,
            opts.timeout,
            opts.max_extracted_bytes,
            opts.max_subprocess_rss_bytes,
        )?;
        return Ok(Some(Extracted {
            text: normalize_extracted_text(&raw),
            truncated,
            source_size_bytes,
        }));
    }

    if let Some(bin) = resolve_tool("antiword") {
        let args: [&OsStr; 1] = [path_os];
        let (raw, truncated) = run_tool_utf8(
            "antiword",
            &bin,
            &args,
            opts.timeout,
            opts.max_extracted_bytes,
            opts.max_subprocess_rss_bytes,
        )?;
        return Ok(Some(Extracted {
            text: normalize_extracted_text(&raw),
            truncated,
            source_size_bytes,
        }));
    }

    Err(ExtractError::ToolMissing { tool: "catdoc" })
}

/// Run `pandoc --from <fmt> --to plain --wrap=none --quiet -- <path>`.
///
/// `--wrap=none` is critical: pandoc otherwise re-wraps at 72 columns,
/// which destroys the paragraph-detection heuristic in the document
/// chunker. `--to plain` (not `markdown`) strips markup overhead — what we
/// want for token-efficient storage. The captured stream is normalized.
pub fn extract_via_pandoc(
    pandoc_format: &str,
    path: &Path,
    opts: &ExtractOptions,
) -> Result<Option<Extracted>, ExtractError> {
    let bin = resolve_tool("pandoc").ok_or(ExtractError::ToolMissing { tool: "pandoc" })?;
    let source_size_bytes = std::fs::metadata(path).map_err(ExtractError::Io)?.len();

    let path_os = path.as_os_str();
    let from_str = std::ffi::OsString::from(pandoc_format);
    let args: [&OsStr; 8] = [
        OsStr::new("--from"),
        from_str.as_os_str(),
        OsStr::new("--to"),
        OsStr::new("plain"),
        OsStr::new("--wrap=none"),
        OsStr::new("--quiet"),
        OsStr::new("--"),
        path_os,
    ];

    let (raw, truncated) = run_tool_utf8(
        "pandoc",
        &bin,
        &args,
        opts.timeout,
        opts.max_extracted_bytes,
        opts.max_subprocess_rss_bytes,
    )?;
    let text = normalize_extracted_text(&raw);
    Ok(Some(Extracted {
        text,
        truncated,
        source_size_bytes,
    }))
}
