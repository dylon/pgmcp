//! Closed vocabulary for the `index_failures` ledger (v42): the *content-
//! intrinsic* reasons a file fails indexing — failures that will recur on every
//! re-walk until the file's bytes change. Per the ADR-003 idiom this is a `TEXT`
//! column + a `CHECK` built from a closed Rust enum via [`sql_in_list`], with a
//! `#[cfg(test)]` golden test pinning the vocabulary — the same idiom as
//! [`crate::tracker::severity`] and [`crate::cron::history::vocab`].
//!
//! Scope (deliberate): only failures that are a property of the *file* are
//! ledgered, because only those benefit from bounded retry. Transient
//! infrastructure failures (a DB `upsert_project` / `replace_indexed_file`
//! timing out, a `get_content_hash` error during an outage) are NOT recorded —
//! they self-heal on the next reconcile pass once the infrastructure recovers,
//! and recording them would mean writing to a possibly-down database. Those keep
//! incrementing the existing `files_failed` counter and surface in the logs.
//!
//! All variants are "bounded": after `[indexer] max_index_retries` attempts on
//! an *unchanged* file (mtime not advanced past the last failure) the scanner
//! stops re-submitting it (see `src/indexer/event_processor.rs`), so a corrupt
//! PDF no longer re-runs `pandoc` on every 30-minute reconcile. Editing the file
//! (mtime advances) lifts the bound and earns a fresh attempt.

use crate::tracker::kind::join_quoted;

/// A content-intrinsic indexing failure recorded in `index_failures.failure_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureKind {
    /// `fs::read_to_string` rejected the file as non-UTF-8 (the extension lied;
    /// e.g. a binary `.py`/`.java` fixture, an AppleDouble `._*` fork).
    NotUtf8,
    /// The document extractor (`pandoc` / `pdftotext` / `ps2ascii`) returned
    /// `None` or errored on the file's content — corrupt or unsupported document.
    DocExtractFailed,
    /// Document extraction exceeded `[indexer] document_extraction_timeout_secs`.
    DocExtractTimeout,
    /// The extraction subprocess was killed (rlimit / OOM); see
    /// `[indexer] max_extraction_subprocess_rss_bytes`.
    DocExtractOom,
}

impl FailureKind {
    /// Canonical ordering; also the source of the DB CHECK vocabulary.
    pub const ALL: &'static [FailureKind] = &[
        Self::NotUtf8,
        Self::DocExtractFailed,
        Self::DocExtractTimeout,
        Self::DocExtractOom,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotUtf8 => "not_utf8",
            Self::DocExtractFailed => "doc_extract_failed",
            Self::DocExtractTimeout => "doc_extract_timeout",
            Self::DocExtractOom => "doc_extract_oom",
        }
    }

    #[allow(dead_code)]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == s)
    }
}

/// SQL `IN (...)` value list built from [`FailureKind::ALL`] — the single source
/// of truth shared with the `index_failures_kind_check` constraint and the
/// scanner's bounded-failure query.
pub fn sql_in_list() -> String {
    join_quoted(FailureKind::ALL.iter().map(|k| k.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn failure_kind_vocabulary_is_pinned() {
        let got: HashSet<&str> = FailureKind::ALL.iter().map(|k| k.as_str()).collect();
        let expected: HashSet<&str> = [
            "not_utf8",
            "doc_extract_failed",
            "doc_extract_timeout",
            "doc_extract_oom",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            got, expected,
            "FailureKind vocabulary drifted from pinned set"
        );
        assert_eq!(FailureKind::ALL.len(), 4);
        assert_eq!(got.len(), 4, "duplicate as_str() value in FailureKind");
    }

    #[test]
    fn parse_roundtrips_for_all() {
        for k in FailureKind::ALL {
            assert_eq!(FailureKind::parse(k.as_str()), Some(*k));
        }
        assert_eq!(FailureKind::parse("nonsense"), None);
    }

    #[test]
    fn sql_in_list_quotes_every_value() {
        let s = sql_in_list();
        assert!(s.contains("'not_utf8'"), "got: {s}");
        assert!(s.contains("'doc_extract_oom'"));
        assert_eq!(s.matches('\'').count(), FailureKind::ALL.len() * 2);
        assert_eq!(s.matches(',').count(), FailureKind::ALL.len() - 1);
    }
}
