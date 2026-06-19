//! Persistent OCR result cache keyed on the source-PDF byte hash.
//!
//! ## Why a dedicated table (not a column on `indexed_files`)
//!
//! `indexed_files.content_hash` is the xxh3 of the *extracted text*, used
//! by the level-2 skip to detect "same extracted output already indexed".
//! For OCR we need to key on the *source PDF bytes* so cache hits work
//! *before* re-running pdftoppm + tesseract. Byte-hash keying also means
//! moving a PDF between projects, worktrees, or disk → HTTP fetch all
//! reuse a single OCR run.
//!
//! ## Sync trait
//!
//! The embed pool worker is synchronous and uses `tokio::runtime::Handle::block_on`
//! at its async-DB seams. The trait below matches that style so
//! `pdf::extract` can call it directly without re-entering tokio. Tests
//! mock the trait with an in-memory `HashMap`.

use std::collections::HashMap;
use std::sync::Mutex;

use sqlx::PgPool;
use tokio::runtime::Handle;
use tracing::error;

/// Read/write surface over the `ocr_extractions` table. Implementations
/// MUST be safe to call from multiple embed-pool worker threads
/// concurrently.
pub trait OcrCache: Send + Sync {
    /// Look up cached OCR text for a given PDF-bytes hash.
    fn lookup(&self, content_hash: i64) -> Option<String>;

    /// Persist a fresh OCR result. Errors are logged but not propagated
    /// because OCR success is more valuable than cache durability — the
    /// caller can still return the text on a transient DB error.
    fn store(
        &self,
        content_hash: i64,
        ocr_text: &str,
        pages_ocred: usize,
        dpi: u32,
        languages: &[String],
    );
}

/// PostgreSQL-backed `OcrCache` impl that bridges the sync trait surface
/// to the async sqlx pool via a `tokio::runtime::Handle`. The handle
/// lives for the duration of the daemon, so capturing it here is safe.
pub struct PgOcrCache {
    pool: PgPool,
    rt: Handle,
}

impl PgOcrCache {
    pub fn new(pool: PgPool, rt: Handle) -> Self {
        Self { pool, rt }
    }
}

impl OcrCache for PgOcrCache {
    fn lookup(&self, content_hash: i64) -> Option<String> {
        let pool = self.pool.clone();
        self.rt
            .block_on(async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT ocr_text FROM ocr_extractions WHERE content_hash = $1",
                )
                .bind(content_hash)
                .fetch_optional(&pool)
                .await
            })
            .unwrap_or_else(|e| {
                error!(error = %e, content_hash, "ocr cache lookup failed");
                None
            })
    }

    fn store(
        &self,
        content_hash: i64,
        ocr_text: &str,
        pages_ocred: usize,
        dpi: u32,
        languages: &[String],
    ) {
        let pool = self.pool.clone();
        let ocr_text = ocr_text.to_string();
        let langs = languages.to_vec();
        let result = self.rt.block_on(async move {
            sqlx::query(
                "INSERT INTO ocr_extractions \
                   (content_hash, ocr_text, pages_ocred, dpi, languages, created_at) \
                 VALUES ($1, $2, $3, $4, $5, NOW()) \
                 ON CONFLICT (content_hash) DO UPDATE SET \
                   ocr_text   = EXCLUDED.ocr_text, \
                   pages_ocred = EXCLUDED.pages_ocred, \
                   dpi        = EXCLUDED.dpi, \
                   languages  = EXCLUDED.languages, \
                   created_at = NOW()",
            )
            .bind(content_hash)
            .bind(&ocr_text)
            .bind(pages_ocred as i32)
            .bind(dpi as i32)
            .bind(&langs)
            .execute(&pool)
            .await
        });
        if let Err(e) = result {
            error!(error = %e, content_hash, "ocr cache store failed");
        }
    }
}

/// In-memory `OcrCache` for tests and for the `refresh_pattern_catalog`
/// HTTP fetch path when no DB pool is available at call time.
#[derive(Default)]
pub struct InMemoryOcrCache {
    inner: Mutex<HashMap<i64, String>>,
}

impl OcrCache for InMemoryOcrCache {
    fn lookup(&self, content_hash: i64) -> Option<String> {
        self.inner.lock().ok()?.get(&content_hash).cloned()
    }

    fn store(
        &self,
        content_hash: i64,
        ocr_text: &str,
        _pages_ocred: usize,
        _dpi: u32,
        _languages: &[String],
    ) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.insert(content_hash, ocr_text.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_round_trip() {
        let cache = InMemoryOcrCache::default();
        assert!(cache.lookup(42).is_none());
        cache.store(42, "hello world", 3, 300, &["eng".to_string()]);
        assert_eq!(cache.lookup(42).as_deref(), Some("hello world"));
    }
}
