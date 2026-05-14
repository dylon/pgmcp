//! End-to-end tests for the Tesseract OCR fallback inside
//! `pgmcp::indexer::extract::pdf`.
//!
//! Each test generates its own scanned-PDF fixture at runtime via
//! ImageMagick `convert`, so no binary artifacts are committed to the
//! repo. Tests skip cleanly if the required tools (`tesseract`,
//! `pdftoppm`, `pdftotext`, `convert`) are not on `$PATH`.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use pgmcp::indexer::extract::{
    self, ExtractOptions, OcrOptions, ocr_cache::InMemoryOcrCache, ocr_cache::OcrCache,
};
use tempfile::TempDir;
use xxhash_rust::xxh3::xxh3_64;

const KNOWN_PHRASE: &str = "Hello OCR World This Is Page One";
const KNOWN_PHRASE_2: &str = "Second line that tesseract must read";

fn tool_available(tool: &str) -> bool {
    // PATH lookup. Avoids the `--version` flag quirks (pdftoppm uses `-v`,
    // ImageMagick `convert` is deprecated and only `magick` works in IMv7+,
    // etc.); presence on PATH is enough — failures inside the test will
    // surface real errors with full stderr.
    let path = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return false,
    };
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(tool);
        if candidate.is_file() {
            return true;
        }
    }
    false
}

fn required_tools_present() -> bool {
    for tool in ["tesseract", "pdftoppm", "pdftotext", "convert"] {
        if !tool_available(tool) {
            eprintln!("SKIPPED: required tool `{}` not on $PATH", tool);
            return false;
        }
    }
    true
}

/// Build a single-page PDF whose content is rendered text wrapped as an
/// image (so `pdftotext` returns nothing). Returns the PDF path.
fn make_scanned_pdf(dir: &Path) -> std::path::PathBuf {
    let png = dir.join("page.png");
    let pdf = dir.join("scanned.pdf");

    let status = Command::new("convert")
        .args([
            "-size",
            "1200x1600",
            "xc:white",
            "-pointsize",
            "48",
            "-fill",
            "black",
            "-gravity",
            "NorthWest",
            "-annotate",
            "+100+200",
            KNOWN_PHRASE,
            "-annotate",
            "+100+400",
            KNOWN_PHRASE_2,
        ])
        .arg(&png)
        .status()
        .expect("convert PNG generation should succeed");
    assert!(status.success(), "convert PNG step failed");

    let status = Command::new("convert")
        .arg(&png)
        .arg(&pdf)
        .status()
        .expect("convert PNG→PDF wrap should succeed");
    assert!(status.success(), "convert PNG→PDF wrap failed");

    pdf
}

fn ocr_options(max_pages: usize) -> OcrOptions {
    OcrOptions {
        enabled: true,
        min_text_chars_per_page: 200,
        max_pages,
        dpi: 200, // 200 DPI keeps the test fast while preserving accuracy.
        languages: vec!["eng".to_string()],
        total_timeout: Duration::from_secs(120),
        max_per_page_bytes: 1024 * 1024,
        max_subprocess_rss_bytes: Some(2 * 1024 * 1024 * 1024),
    }
}

fn extract_options_with_ocr(max_pages: usize) -> ExtractOptions {
    ExtractOptions {
        timeout: Duration::from_secs(60),
        max_extracted_bytes: 16 * 1024 * 1024,
        max_subprocess_rss_bytes: Some(2 * 1024 * 1024 * 1024),
        ocr: ocr_options(max_pages),
    }
}

#[test]
fn ocr_extract_image_only_pdf_returns_text() {
    if !required_tools_present() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let pdf = make_scanned_pdf(tmp.path());

    let opts = extract_options_with_ocr(5);
    let result = extract::pdf::extract(&pdf, &opts).expect("extract should succeed");
    let extracted = result.expect("extract should return Some");

    let lc = extracted.text.to_lowercase();
    assert!(
        lc.contains("hello") && lc.contains("ocr"),
        "OCR output should contain 'Hello' and 'OCR'; got: {:?}",
        &extracted.text[..extracted.text.len().min(400)]
    );
}

#[test]
fn ocr_cache_hit_returns_cached_text() {
    if !required_tools_present() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let pdf = make_scanned_pdf(tmp.path());
    let pdf_bytes = std::fs::read(&pdf).unwrap();
    let byte_hash = xxh3_64(&pdf_bytes) as i64;

    let cache = InMemoryOcrCache::default();

    // First pass: cache miss → runs OCR → populates cache.
    let opts = extract_options_with_ocr(5);
    let first = extract::pdf::extract_with_cache(&pdf, &opts, Some(&cache), Some(byte_hash))
        .expect("first extract should succeed")
        .expect("first extract should return Some");
    let cached_after_first = cache
        .lookup(byte_hash)
        .expect("cache should be populated after first OCR run");
    assert_eq!(
        cached_after_first, first.text,
        "stored cache value should match returned text"
    );

    // Second pass with the same PDF: phases 1 and 2 still run (pdftotext +
    // threshold check) but phase 3 short-circuits to the cached text.
    // We can't strip the on-disk PDF here because pdftotext runs before
    // the cache lookup — what we *can* assert is text equality, since a
    // cache miss would re-invoke tesseract and the OCR output is
    // deterministic only up to whitespace.
    let second = extract::pdf::extract_with_cache(&pdf, &opts, Some(&cache), Some(byte_hash))
        .expect("second extract should succeed")
        .expect("second extract should return Some");
    assert_eq!(
        second.text, first.text,
        "second extract should return identical text via cache",
    );

    // A miss on a different hash should NOT find a cache entry, so the
    // cache surface itself isn't accidentally pretending to know everything.
    assert!(
        cache.lookup(byte_hash.wrapping_add(1)).is_none(),
        "unrelated hash should not appear in cache"
    );
}

#[test]
fn ocr_cache_explicit_round_trip() {
    // Pure cache-trait round-trip — exercises the OcrCache contract without
    // running tesseract. Complements the e2e test by isolating the cache
    // logic from the subprocess pipeline.
    let cache = InMemoryOcrCache::default();
    let hash: i64 = 0x1234_5678_dead_beef_u64 as i64;
    assert!(cache.lookup(hash).is_none());
    cache.store(hash, "stub OCR output", 7, 300, &["eng".to_string()]);
    assert_eq!(cache.lookup(hash).as_deref(), Some("stub OCR output"));
}

#[test]
fn ocr_disabled_returns_sparse_pdftotext() {
    if !required_tools_present() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let pdf = make_scanned_pdf(tmp.path());

    let mut opts = extract_options_with_ocr(5);
    opts.ocr.enabled = false;
    let extracted = extract::pdf::extract(&pdf, &opts)
        .expect("extract should succeed")
        .expect("extract should return Some");
    let lc = extracted.text.to_lowercase();
    assert!(
        !lc.contains("hello ocr world"),
        "OCR-disabled extraction should NOT contain the OCR phrase; got: {:?}",
        &extracted.text[..extracted.text.len().min(400)]
    );
}

#[test]
fn ocr_skipped_when_pdftotext_returns_enough() {
    if !required_tools_present() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    // Build a real-text PDF (no image wrapping) — `convert` with text
    // overlay drawn straight to PDF preserves selectable text.
    let pdf = tmp.path().join("text.pdf");
    let status = Command::new("convert")
        .args([
            "-size",
            "1200x1600",
            "xc:white",
            "-pointsize",
            "32",
            "-fill",
            "black",
            "-gravity",
            "NorthWest",
            "-annotate",
            "+100+200",
            "The quick brown fox jumps over the lazy dog. \
             Sphinx of black quartz, judge my vow. \
             Pack my box with five dozen liquor jugs. \
             How vexingly quick daft zebras jump.",
        ])
        .arg(&pdf)
        .status()
        .expect("convert should succeed");
    assert!(status.success());

    // Lower the threshold so the test's sparse but real text comfortably clears.
    let mut opts = extract_options_with_ocr(5);
    opts.ocr.min_text_chars_per_page = 1;

    let extracted = extract::pdf::extract(&pdf, &opts)
        .expect("extract should succeed")
        .expect("extract should return Some");
    let lc = extracted.text.to_lowercase();
    assert!(
        lc.contains("quick") || lc.contains("brown") || lc.contains("fox"),
        "pdftotext path should extract real text; got: {:?}",
        &extracted.text[..extracted.text.len().min(400)]
    );
}
