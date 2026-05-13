// Until Phase 5 wires this module into the embed/pool pipeline, the
// public surface looks like dead code to rustc. The submodules are
// independently testable and useful in isolation; relaxing the lint at
// the module root keeps Phase 2 landings green without scattering
// per-item `#[allow]` attributes that we'd just have to remove later.
#![allow(dead_code)]

//! Document extraction module.
//!
//! Routes binary document formats (PDF, PostScript, DOCX, DOC, RTF, ODT,
//! EPUB) and high-markup text formats (LaTeX, ORG) through external CLI
//! tools (`pdftotext`, `ps2ascii`, `pandoc`) to produce normalized plain
//! text suitable for chunking and embedding. Plain text formats (RST,
//! BibTeX, TXT) are read directly with BOM stripping and the same
//! normalization pass for uniformity.
//!
//! ## Design
//!
//! - **Subprocess strategy**: each tool is invoked once per file with a
//!   bounded timeout (`document_extraction_timeout_secs`) and a bounded
//!   output size (`max_extracted_text_bytes`). Hangs and runaway outputs
//!   are killed rather than allowed to wedge an embed worker.
//! - **Tool availability** is resolved lazily via a per-tool `OnceLock`.
//!   When a tool is missing, every call returns `ExtractError::ToolMissing`
//!   without re-running `which`; the daemon's startup preflight logs the
//!   missing tool exactly once.
//! - **Normalization** is applied unconditionally (`normalize.rs`) so the
//!   storage representation is the smallest UTF-8 form that preserves
//!   meaning — this is the layer that makes MCP tool results
//!   token-efficient for Claude/Codex without any wire-format trickery.
//!
//! The dispatcher returns `Ok(None)` for languages it doesn't recognize as
//! "document" languages, signaling the caller (the indexing pipeline) to
//! fall through to the existing `std::fs::read_to_string` code path.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

pub mod normalize;
pub mod office;
pub mod pdf;
pub mod postscript;
pub mod subprocess;
pub mod utf8;

/// Output of a successful extraction.
#[derive(Debug, Clone)]
pub struct Extracted {
    /// Normalized UTF-8 plain text. Already passed through
    /// `normalize::normalize_extracted_text`.
    pub text: String,
    /// True when extraction was truncated at `max_extracted_bytes` or when
    /// the underlying tool was killed because output exceeded the cap.
    pub truncated: bool,
    /// Source file size in bytes, recorded for the Level-1 metadata skip.
    pub source_size_bytes: u64,
}

/// Knobs for a single extraction call. Sourced from `IndexerConfig` (or
/// a `ProjectIndexerOverride`) at the call site.
#[derive(Debug, Clone, Copy)]
pub struct ExtractOptions {
    pub timeout: Duration,
    pub max_extracted_bytes: usize,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            max_extracted_bytes: 50 * 1024 * 1024,
        }
    }
}

/// Errors surfaced by the dispatcher. `ToolMissing` is a soft failure —
/// the caller should count it and move on rather than abort the daemon.
#[derive(Debug)]
pub enum ExtractError {
    /// The required CLI tool is not on `$PATH`. Treated as a per-file
    /// soft failure: the pipeline records a skip-counter increment.
    ToolMissing {
        tool: &'static str,
    },
    /// The subprocess exceeded `timeout` and was killed.
    Timeout,
    /// Extraction reached `max_extracted_bytes`; the partial result is
    /// returned with `truncated = true` and this error variant is NOT
    /// produced (`SizeCap` is reserved for future hard failures).
    #[allow(dead_code)]
    SizeCap,
    Io(std::io::Error),
    Process {
        tool: &'static str,
        status: i32,
        stderr: String,
    },
    Encoding(String),
}

impl std::fmt::Display for ExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolMissing { tool } => write!(f, "CLI tool missing: {tool}"),
            Self::Timeout => write!(f, "extraction timed out"),
            Self::SizeCap => write!(f, "extraction exceeded size cap"),
            Self::Io(e) => write!(f, "I/O error during extraction: {e}"),
            Self::Process {
                tool,
                status,
                stderr,
            } => {
                write!(f, "{tool} exited with status {status}: {stderr}")
            }
            Self::Encoding(msg) => write!(f, "encoding error: {msg}"),
        }
    }
}

impl std::error::Error for ExtractError {}

/// Dispatch table.
///
/// Returns `Ok(None)` for languages that are *not* documents — the caller
/// reads the file directly via `std::fs::read_to_string`. Returns
/// `Ok(Some(_))` for any language the document pipeline handles.
pub fn extract_for_language(
    language: &str,
    path: &Path,
    opts: &ExtractOptions,
) -> Result<Option<Extracted>, ExtractError> {
    match language {
        "pdf" => pdf::extract(path, opts),
        "postscript" => postscript::extract(path, opts),
        "docx" | "doc" | "rtf" | "odt" | "epub" => office::extract_office(language, path, opts),
        // LaTeX and ORG ride pandoc to drop markup overhead at index time,
        // delivering ~30-50% token reduction vs storing raw markup.
        "latex" => office::extract_via_pandoc("latex", path, opts),
        "org" => office::extract_via_pandoc("org", path, opts),
        "rst" | "bibtex" | "text" => utf8::read(path, opts),
        _ => Ok(None),
    }
}

/// True when the language is handled by the extraction pipeline. Mirrors
/// the dispatcher arm. Callers use this to decide whether to apply the
/// `max_document_source_bytes` source-byte gate vs the regular
/// `max_file_size_bytes` gate.
pub fn is_document_language(language: &str) -> bool {
    matches!(
        language,
        "pdf"
            | "postscript"
            | "docx"
            | "doc"
            | "rtf"
            | "odt"
            | "epub"
            | "latex"
            | "org"
            | "rst"
            | "bibtex"
            | "text"
    )
}

/// True when the language requires a CLI tool subprocess (i.e. extraction
/// can fail with `ToolMissing`). Plain-text passthrough languages (rst,
/// bibtex, text) are NOT in this set.
#[allow(dead_code)]
pub fn requires_external_tool(language: &str) -> bool {
    matches!(
        language,
        "pdf" | "postscript" | "docx" | "doc" | "rtf" | "odt" | "epub" | "latex" | "org"
    )
}

/// Lazy per-tool `which::which` resolution.
///
/// First call performs the lookup; subsequent calls return the cached
/// result. A missing tool is cached as `None` so we don't repeatedly hit
/// the filesystem from inside the embed pool's hot loop.
pub(crate) fn resolve_tool(tool: &'static str) -> Option<PathBuf> {
    let cache = tool_cache(tool);
    cache.get_or_init(|| which::which(tool).ok()).clone()
}

fn tool_cache(tool: &'static str) -> &'static OnceLock<Option<PathBuf>> {
    static PDFTOTEXT: OnceLock<Option<PathBuf>> = OnceLock::new();
    static PS2ASCII: OnceLock<Option<PathBuf>> = OnceLock::new();
    static PANDOC: OnceLock<Option<PathBuf>> = OnceLock::new();
    match tool {
        "pdftotext" => &PDFTOTEXT,
        "ps2ascii" => &PS2ASCII,
        "pandoc" => &PANDOC,
        // Defensive: an unknown tool gets a fresh-but-immediately-leaked
        // OnceLock. This branch is never hit in production because every
        // call site uses a literal known to this match.
        _ => Box::leak(Box::new(OnceLock::new())),
    }
}

/// Metadata for daemon startup preflight. Each entry is
/// `(tool_name, languages_affected, install_hint)`.
#[allow(dead_code)]
pub const REQUIRED_TOOLS: &[(&str, &[&str], &str)] = &[
    (
        "pdftotext",
        &["pdf"],
        "install poppler / poppler-utils (Arch: pacman -S poppler; \
         Debian: apt install poppler-utils; macOS: brew install poppler)",
    ),
    (
        "ps2ascii",
        &["postscript"],
        "install ghostscript (Arch: pacman -S ghostscript; \
         Debian: apt install ghostscript; macOS: brew install ghostscript)",
    ),
    (
        "pandoc",
        &["docx", "doc", "rtf", "odt", "epub", "latex", "org"],
        "install pandoc (Arch: pacman -S pandoc-cli; \
         Debian: apt install pandoc; macOS: brew install pandoc)",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatcher_returns_none_for_unknown_language() {
        let opts = ExtractOptions::default();
        let result = extract_for_language("python", Path::new("/tmp/nope.py"), &opts).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn is_document_language_matches_dispatcher() {
        for lang in [
            "pdf",
            "postscript",
            "docx",
            "doc",
            "rtf",
            "odt",
            "epub",
            "latex",
            "org",
            "rst",
            "bibtex",
            "text",
        ] {
            assert!(is_document_language(lang), "expected {lang} to be document");
        }
        for lang in ["rust", "python", "markdown", "jsonl", "shell"] {
            assert!(
                !is_document_language(lang),
                "expected {lang} to NOT be document"
            );
        }
    }

    #[test]
    fn requires_external_tool_excludes_passthrough() {
        assert!(requires_external_tool("pdf"));
        assert!(requires_external_tool("latex"));
        assert!(!requires_external_tool("rst"));
        assert!(!requires_external_tool("text"));
    }
}
