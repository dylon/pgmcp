//! Tesseract OCR fallback for scanned PDFs.
//!
//! Invoked from `pdf::extract` when `pdftotext -layout` returns less than
//! `ocr_min_text_chars_per_page * page_count` characters — the
//! classic signature of an image-only PDF. Rasterizes the PDF with
//! `pdftoppm -r <dpi> -png` and runs `tesseract` per page, concatenating
//! the result.
//!
//! ## Design notes
//!
//! * **Sequential per document, parallel across documents.** The embed
//!   pool already owns parallelism via `pool_size` workers. Forking N
//!   tesseract children from one worker would (a) stampede the
//!   `max_subprocess_rss_bytes` budget (rlimit is per-child, not
//!   per-tree), (b) make deadlines hard to reason about, (c) push N×16
//!   threads through the kernel scheduler. Throughput scales by raising
//!   `embeddings.pool_size`, not by per-document fork-join.
//! * **`OMP_THREAD_LIMIT=1`** is forced into the tesseract child env so
//!   internal OpenMP parallelism doesn't compound the previous point.
//! * Subprocesses reuse `subprocess::run_bounded`, inheriting timeout,
//!   stdout cap, and rlimit machinery already exercised by pdftotext.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tracing::{debug, warn};

use super::subprocess::run_bounded;
use super::{ExtractError, OcrOptions, resolve_tool};

/// Output of an OCR run.
///
/// An empty `text` is a real outcome (image-only page that tesseract
/// could not read); the caller decides whether to fall back to the
/// sparse `pdftotext` output.
#[derive(Debug)]
pub struct OcrResult {
    pub text: String,
    pub pages_ocred: usize,
    pub truncated_by_max_pages: bool,
    pub truncated_by_deadline: bool,
}

/// Run OCR on every page of a PDF in sequence. Caller has already
/// determined `pdftotext` produced too little text.
pub fn run_ocr(
    pdf_path: &Path,
    page_count: usize,
    opts: &OcrOptions,
) -> Result<OcrResult, ExtractError> {
    let pdftoppm =
        resolve_tool("pdftoppm").ok_or(ExtractError::ToolMissing { tool: "pdftoppm" })?;
    let tesseract =
        resolve_tool("tesseract").ok_or(ExtractError::ToolMissing { tool: "tesseract" })?;

    let tmp = TempDir::new().map_err(ExtractError::Io)?;
    let prefix = tmp.path().join("page");
    let pages_target = page_count.max(1).min(opts.max_pages);
    let truncated_by_max_pages = pages_target < page_count;
    let deadline = Instant::now() + opts.total_timeout;

    rasterize(&pdftoppm, pdf_path, &prefix, opts, pages_target, deadline)?;

    let lang_arg = if opts.languages.is_empty() {
        String::from("eng")
    } else {
        opts.languages.join("+")
    };
    let mut combined = String::with_capacity(8 * 1024 * pages_target);
    let mut pages_done = 0usize;
    let mut truncated_by_deadline = false;
    let pad = digit_width(pages_target);

    for page in 1..=pages_target {
        if Instant::now() >= deadline {
            truncated_by_deadline = true;
            break;
        }
        let image = locate_page_image(tmp.path(), page, pad);
        let Some(image_path) = image else {
            warn!(
                pdf = %pdf_path.display(),
                page,
                "expected PNG missing after pdftoppm; skipping page"
            );
            continue;
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        let page_text = ocr_one_page(&tesseract, &image_path, &lang_arg, opts, remaining)?;
        let trimmed = page_text.trim();
        if !trimmed.is_empty() {
            if !combined.is_empty() {
                combined.push_str("\n\n");
            }
            combined.push_str(trimmed);
        }
        pages_done += 1;
    }

    debug!(
        pdf = %pdf_path.display(),
        pages_done,
        truncated_by_max_pages,
        truncated_by_deadline,
        text_chars = combined.chars().count(),
        "OCR completed",
    );

    Ok(OcrResult {
        text: combined,
        pages_ocred: pages_done,
        truncated_by_max_pages,
        truncated_by_deadline,
    })
}

fn rasterize(
    pdftoppm: &Path,
    pdf: &Path,
    out_prefix: &Path,
    opts: &OcrOptions,
    last_page: usize,
    deadline: Instant,
) -> Result<(), ExtractError> {
    let dpi = opts.dpi.to_string();
    let last = last_page.to_string();
    let mut cmd = Command::new(pdftoppm);
    cmd.args([
        OsStr::new("-r"),
        OsStr::new(&dpi),
        OsStr::new("-png"),
        OsStr::new("-f"),
        OsStr::new("1"),
        OsStr::new("-l"),
        OsStr::new(&last),
        pdf.as_os_str(),
        out_prefix.as_os_str(),
    ]);
    let timeout = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_secs(1));
    // pdftoppm writes files, not stdout — a 64 KiB stdout cap is generous.
    let captured = run_bounded(
        cmd,
        "pdftoppm",
        timeout,
        64 * 1024,
        opts.max_subprocess_rss_bytes,
    )?;

    // Verify pdftoppm actually produced PNGs. Poppler frequently exits 0
    // on corrupt/encrypted PDFs ("Syntax Error: Couldn't find trailer
    // dictionary" on stderr) without emitting any output. Without this
    // check, the per-page loop above this function reports
    // `pages_target` × "expected PNG missing" warnings for what is in
    // reality a single, structural pdftoppm failure. We surface that
    // upstream as an explicit error so pdf.rs can cache the negative
    // result and skip OCR on subsequent rescans.
    let parent = out_prefix.parent().unwrap_or(out_prefix);
    let png_count = count_pngs_in(parent);
    if png_count == 0 {
        let stderr_tail = String::from_utf8_lossy(&captured.stderr);
        let stderr_tail = stderr_tail.trim();
        warn!(
            pdf = %pdf.display(),
            stderr = %stderr_tail,
            "pdftoppm produced no PNGs; marking PDF as un-OCR-able",
        );
        return Err(ExtractError::OcrFailed(Box::new(ExtractError::Process {
            tool: "pdftoppm",
            status: 0,
            stderr: format!("zero PNGs written; stderr: {stderr_tail}"),
        })));
    }
    Ok(())
}

/// Count `*.png` files in `dir`. Returns 0 on read-dir errors (the
/// caller already has a clear signal that the rasterize failed). This
/// is a directory listing, not a stat per page — pdftoppm-emitted PNGs
/// are the only contents of the tempdir so the count is exact.
fn count_pngs_in(dir: &Path) -> usize {
    let Ok(read) = std::fs::read_dir(dir) else {
        return 0;
    };
    read.filter_map(|e| e.ok())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
        })
        .count()
}

fn ocr_one_page(
    tesseract: &Path,
    image: &Path,
    lang: &str,
    opts: &OcrOptions,
    remaining: Duration,
) -> Result<String, ExtractError> {
    let mut cmd = Command::new(tesseract);
    cmd.args([
        image.as_os_str(),
        OsStr::new("stdout"),
        OsStr::new("-l"),
        OsStr::new(lang),
        OsStr::new("--psm"),
        OsStr::new("1"),
    ]);
    // Tesseract's internal OpenMP threading multiplies cleanly past the
    // embed pool's parallelism budget. Cap at 1 thread per child so N
    // pool workers don't each fork 16 threads on a 32-thread box.
    cmd.env("OMP_THREAD_LIMIT", "1");

    let timeout = remaining.max(Duration::from_secs(5));
    let cap = run_bounded(
        cmd,
        "tesseract",
        timeout,
        opts.max_per_page_bytes,
        opts.max_subprocess_rss_bytes,
    )?;
    Ok(String::from_utf8_lossy(&cap.stdout).into_owned())
}

/// pdftoppm produces `page-<N>.png` where `<N>` is left-zero-padded to
/// match the digit count of the highest page number requested. Try the
/// padded form first; fall back to the unpadded form for compatibility
/// with older poppler builds.
fn locate_page_image(dir: &Path, page: usize, pad_width: usize) -> Option<PathBuf> {
    let padded = dir.join(format!("page-{page:0pad$}.png", pad = pad_width));
    if padded.exists() {
        return Some(padded);
    }
    let unpadded = dir.join(format!("page-{page}.png"));
    if unpadded.exists() {
        return Some(unpadded);
    }
    None
}

fn digit_width(n: usize) -> usize {
    if n < 10 {
        1
    } else if n < 100 {
        2
    } else if n < 1000 {
        3
    } else if n < 10_000 {
        4
    } else {
        // Pathological — 10k+ page document. Pad generously.
        n.to_string().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    #[test]
    fn digit_width_matches_pdftoppm_padding() {
        assert_eq!(digit_width(1), 1);
        assert_eq!(digit_width(9), 1);
        assert_eq!(digit_width(10), 2);
        assert_eq!(digit_width(99), 2);
        assert_eq!(digit_width(100), 3);
        assert_eq!(digit_width(999), 3);
        assert_eq!(digit_width(1000), 4);
        assert_eq!(digit_width(9999), 4);
        assert_eq!(digit_width(10_000), 5);
    }

    #[test]
    fn count_pngs_in_empty_dir_returns_zero() {
        // The zero-PNG case is precisely the signal we use to detect
        // pdftoppm's silent failure on corrupt/encrypted PDFs.
        let tmp = TempDir::new().expect("tempdir");
        assert_eq!(count_pngs_in(tmp.path()), 0);
    }

    #[test]
    fn count_pngs_in_counts_only_png_extension() {
        // pdftoppm emits page-N.png; the tempdir is exclusive to one
        // rasterize() call so we don't expect other artifacts, but the
        // helper must still ignore unrelated entries (e.g. an editor's
        // backup file, or a tempdir-level marker file).
        let tmp = TempDir::new().expect("tempdir");
        for name in [
            "page-1.png",
            "page-02.png",
            "page-3.PNG", // case-insensitive: ascii-eq covers this
            "notes.txt",
            "scratch.jpg",
        ] {
            File::create(tmp.path().join(name)).expect("create");
        }
        assert_eq!(count_pngs_in(tmp.path()), 3);
    }

    #[test]
    fn count_pngs_in_returns_zero_for_missing_dir() {
        // Defensive: a missing parent (race with cleanup) yields 0 so
        // the rasterize() failure path treats it the same as "no PNGs
        // written" — which is the right semantics.
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("does-not-exist");
        assert_eq!(count_pngs_in(&path), 0);
    }
}
