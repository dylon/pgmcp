//! Repo-wide regression guard for the logging-level convention (ADR-021):
//! a swallowed/caught runtime error must log at `error!`, not `warn!`. Under a
//! `[logging] level = "error"` (or `RUST_LOG=error`) posture — common in
//! production — every `warn!` event is silently dropped, so a runtime error
//! mis-logged at `warn!` is invisible. Two real incidents (the topic
//! algo-signature staleness and the index-freshness false-staleness bugs) hid
//! in exactly such `warn!("… failed")` lines.
//!
//! This test scans every `warn!(` invocation in `src/` (balancing parens, so
//! multi-line macros are covered) and FAILS if the invocation text contains a
//! high-precision swallowed-error trigger phrase (`failed`, `could not`,
//! `panicked`, `falling back`) — unless the site is on the documented
//! allow-list below. The allow-list holds the deliberately-retained `warn!`
//! sites whose message legitimately contains a trigger word (graceful
//! degradations, transient retries, config advisories, findings reports). A
//! *new* swallowed-error `warn!` anywhere else is caught.

use std::fs;
use std::path::{Path, PathBuf};

/// Trigger phrases (lowercased) that mark a `warn!` message as a swallowed
/// runtime error. Kept high-precision to avoid false positives.
const TRIGGERS: &[&str] = &["failed", "could not", "panicked", "falling back"];

/// Deliberately-retained `warn!` sites whose message contains a trigger word
/// but which are, per ADR-021, expected/benign (graceful degradation, transient
/// retry handled internally, config advisory, or a findings report). Matched as
/// a substring of the macro invocation text. Each entry has a one-line reason.
const ALLOWED: &[&str] = &[
    // Startup soft-guards with sane fallback (continue / default applied).
    "fuzzy format-version guard failed (non-fatal; continuing)",
    "Failed to read active embedding signature; continuing",
    // Shutdown-time sweep (low-consequence; daemon is already stopping).
    "Heavy-backend shutdown sweep failed",
    // Expected fallback for an unsupported phonetic language.
    "no rule pack for language; falling back to English",
    // LMDB centroid-cache miss → FCM recomputes from scratch (perf, not error).
    "Failed to open topic LMDB store",
    "LMDB load_centroids failed",
    "LMDB store_centroids failed",
    // Config-file advisories with a built-in-default fallback.
    "client_profiles.toml parse failed",
    "client_profiles.toml read failed",
    // target-cleanup graceful degradations (documented dry-run-first design).
    "list_projects failed; relying on configured roots only",
    "tmp provenance query failed; tmp sweep falls back to age-only",
];

#[test]
fn no_warn_logs_a_swallowed_error() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("src");
    assert!(src.is_dir(), "src dir missing at {}", src.display());

    let mut files = Vec::new();
    collect_rs(&src, &mut files);

    let mut violations: Vec<String> = Vec::new();
    for path in files {
        let text = fs::read_to_string(&path).expect("read src file");
        for (start, _) in text.match_indices("warn!(") {
            // Skip occurrences inside a line comment (e.g. doc examples).
            if on_comment_line(&text, start) {
                continue;
            }
            let invocation = macro_invocation(&text, start);
            let lower = invocation.to_ascii_lowercase();
            if !TRIGGERS.iter().any(|t| lower.contains(t)) {
                continue;
            }
            if ALLOWED.iter().any(|a| invocation.contains(a)) {
                continue;
            }
            let line_no = text[..start].bytes().filter(|&b| b == b'\n').count() + 1;
            let snippet: String = invocation.chars().take(120).collect();
            violations.push(format!(
                "{}:{}: {}",
                path.display(),
                line_no,
                snippet
                    .replace('\n', " ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "swallowed-error `warn!` found (ADR-021: use `error!` for caught runtime \
         errors / degraded-after-failure fallbacks; only expected/benign conditions \
         stay `warn!`). If a new site is genuinely benign, add it to ALLOWED with a \
         reason:\n{}",
        violations.join("\n")
    );
}

/// True if the `warn!(` at `start` sits on a line whose first non-blank content
/// is a line comment (`//`).
fn on_comment_line(text: &str, start: usize) -> bool {
    let line_start = text[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    text[line_start..start].trim_start().starts_with("//")
}

/// Return the full `warn!( … )` invocation text starting at `start`, balancing
/// parentheses and skipping over string literals (so parens inside a message do
/// not unbalance the scan). Falls back to the file tail if unbalanced.
fn macro_invocation(text: &str, start: usize) -> &str {
    let bytes = text.as_bytes();
    // `start` indexes the 'w' of "warn!("; the '(' is at start + 5.
    let open = start + 5;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    let mut i = open;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else {
            match b {
                b'"' => in_str = true,
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return &text[start..=i];
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    &text[start..]
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}
