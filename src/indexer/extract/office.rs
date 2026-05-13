use std::ffi::OsStr;
use std::path::Path;

use super::{
    ExtractError, ExtractOptions, Extracted, normalize::normalize_extracted_text, resolve_tool,
    subprocess::run_tool_utf8,
};

/// Extract plain text from a binary office-format file via `pandoc`.
///
/// `language` must be one of: `docx`, `doc`, `rtf`, `odt`, `epub`.
/// Behavior diverges per format only in the `--from` flag passed to pandoc.
pub fn extract_office(
    language: &str,
    path: &Path,
    opts: &ExtractOptions,
) -> Result<Option<Extracted>, ExtractError> {
    let pandoc_format = match language {
        "docx" => "docx",
        "doc" => "doc",
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
    )?;
    let text = normalize_extracted_text(&raw);
    Ok(Some(Extracted {
        text,
        truncated,
        source_size_bytes,
    }))
}
