use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use tracing::{debug, warn};

use super::{
    ExtractError, ExtractOptions, Extracted, normalize::normalize_extracted_text, ocr,
    ocr_cache::OcrCache, resolve_tool, subprocess::run_bounded, subprocess::run_tool_utf8,
};

/// Extract PDF text via `pdftotext` (poppler-utils), falling back to
/// Tesseract OCR when the extracted text is too sparse for the document
/// to plausibly be a real text PDF.
///
/// `pdftotext` flags chosen for accuracy + reading-order fidelity on
/// multi-column papers:
///
/// * `-layout` — preserve physical layout (multi-column reading order).
/// * `-enc UTF-8` — force UTF-8 output regardless of locale.
/// * `-q` — suppress startup/banner noise on stderr.
/// * `-nopgbrk` — drop form-feed page-break characters; the normalization
///   pass strips them anyway, but `-nopgbrk` keeps the captured stream
///   cleaner up-front.
///
/// Output is normalized (NFKC, dehyphenation, page-number strip,
/// whitespace collapse) before being returned. When `opts.ocr.enabled` is
/// true and the pdftotext output falls below
/// `ocr.min_text_chars_per_page * page_count`, the OCR fallback kicks in
/// (see `extract::ocr` and `extract::ocr_cache`).
pub fn extract(path: &Path, opts: &ExtractOptions) -> Result<Option<Extracted>, ExtractError> {
    extract_with_cache(path, opts, None, None)
}

/// Identical to [`extract`] but accepts an OCR cache + source-bytes hash
/// so callers in the embed pool can reuse OCR runs across re-indexes,
/// project clones, or HTTP fetches of the same PDF.
pub fn extract_with_cache(
    path: &Path,
    opts: &ExtractOptions,
    cache: Option<&dyn OcrCache>,
    content_hash: Option<i64>,
) -> Result<Option<Extracted>, ExtractError> {
    let bin = resolve_tool("pdftotext").ok_or(ExtractError::ToolMissing { tool: "pdftotext" })?;
    let source_size_bytes = std::fs::metadata(path).map_err(ExtractError::Io)?.len();

    // ---- Phase 1: vanilla pdftotext ----------------------------------
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
        opts.max_subprocess_rss_bytes,
    )?;
    let pdftotext_text = normalize_extracted_text(&raw);

    // ---- Phase 2: decide whether to OCR ------------------------------
    if !opts.ocr.enabled {
        return Ok(Some(Extracted {
            text: pdftotext_text,
            truncated,
            source_size_bytes,
        }));
    }
    let page_count = count_pdf_pages(path).unwrap_or(1).max(1);
    let threshold = opts.ocr.min_text_chars_per_page.saturating_mul(page_count);
    let observed_chars = pdftotext_text.chars().count();
    if observed_chars >= threshold {
        return Ok(Some(Extracted {
            text: pdftotext_text,
            truncated,
            source_size_bytes,
        }));
    }

    // ---- Phase 3: cache lookup ---------------------------------------
    if let (Some(cache), Some(hash)) = (cache, content_hash)
        && let Some(cached) = cache.lookup(hash)
    {
        debug!(path = %path.display(), content_hash = hash, "OCR cache hit");
        return Ok(Some(Extracted {
            text: cached,
            truncated: false,
            source_size_bytes,
        }));
    }

    // ---- Phase 4: OCR ------------------------------------------------
    debug!(
        path = %path.display(),
        page_count,
        pdftotext_chars = observed_chars,
        threshold,
        "PDF text below OCR threshold; running tesseract",
    );
    let ocr_result = match ocr::run_ocr(path, page_count, &opts.ocr) {
        Ok(r) => r,
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "OCR failed; falling back to sparse pdftotext output",
            );
            // Cache the negative outcome (empty text) so subsequent
            // rescans short-circuit at Phase 3 instead of re-attempting
            // OCR on a structurally unreadable PDF. Without this, the
            // same poisoned PDFs (corrupt headers, encrypted, etc.)
            // produced 50× "expected PNG missing" warns *per rescan*,
            // observed as 150 warns/day from 3 PDFs. Cache invalidation
            // is automatic: any byte-level change to the PDF flips
            // `content_hash`, invalidating this entry.
            if let (Some(cache), Some(hash)) = (cache, content_hash) {
                cache.store(hash, "", 0, opts.ocr.dpi, &opts.ocr.languages);
            }
            return Ok(Some(Extracted {
                text: pdftotext_text,
                truncated,
                source_size_bytes,
            }));
        }
    };
    let normalized = normalize_extracted_text(&ocr_result.text);
    if normalized.trim().is_empty() {
        debug!(
            path = %path.display(),
            "OCR produced no text; returning sparse pdftotext output",
        );
        // Same negative-result caching as the error branch — tesseract
        // succeeded but the document is genuinely unreadable (image-only
        // PDF that OCR can't crack), so don't bother retrying next time.
        if let (Some(cache), Some(hash)) = (cache, content_hash) {
            cache.store(hash, "", 0, opts.ocr.dpi, &opts.ocr.languages);
        }
        return Ok(Some(Extracted {
            text: pdftotext_text,
            truncated,
            source_size_bytes,
        }));
    }

    // ---- Phase 5: cache store ----------------------------------------
    if let (Some(cache), Some(hash)) = (cache, content_hash) {
        cache.store(
            hash,
            &normalized,
            ocr_result.pages_ocred,
            opts.ocr.dpi,
            &opts.ocr.languages,
        );
    }

    Ok(Some(Extracted {
        text: normalized,
        truncated: truncated
            || ocr_result.truncated_by_max_pages
            || ocr_result.truncated_by_deadline,
        source_size_bytes,
    }))
}

/// Best-effort page count via `pdfinfo`. Falls back to `None` when the
/// tool is unavailable or the output is unparseable; callers treat the
/// document as single-page for OCR-budget purposes.
fn count_pdf_pages(path: &Path) -> Option<usize> {
    let pdfinfo = resolve_tool("pdfinfo")?;
    let mut cmd = Command::new(pdfinfo);
    cmd.arg(path);
    let captured = run_bounded(
        cmd,
        "pdfinfo",
        Duration::from_secs(5),
        16 * 1024,
        Some(256 * 1024 * 1024),
    )
    .ok()?;
    let stdout = String::from_utf8_lossy(&captured.stdout);
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("Pages:") {
            return rest.trim().parse().ok();
        }
    }
    None
}
