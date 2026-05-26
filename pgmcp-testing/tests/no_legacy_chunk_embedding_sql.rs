//! Repo-wide regression guard: no SQL in `src/` may reference the legacy chunk
//! `embedding` column via the common `file_chunks`-family aliases
//! (`c` / `ca` / `cb` / `c2`). Post-`embed-cutover --drop-legacy` that column
//! is dropped (BGE-M3 `embedding_v2` is canonical), so such a reference throws
//! `column "embedding" does not exist` at runtime (the boot/topic-clustering/
//! similarity failures fixed on 2026-05-26). Use the active-signature column
//! instead: `read_active_signature(pool).await?.read_column()` then
//! `format!("… {col} …")`.
//!
//! 1024d-direct tables (`memory_observations`, `durable_mandates`,
//! `session_mandates`, `memory_unified_nodes`) keep a canonical `embedding`
//! column and use other aliases / no alias, so a bare `embedding` or
//! `o.embedding` is intentionally NOT matched here. Likewise
//! `c.embedding_v2` / `c.embedding_signature` are excluded by the trailing
//! word-boundary check.

use std::fs;
use std::path::{Path, PathBuf};

/// Files allowed to contain a matched pattern. Empty: every chunk-alias
/// embedding read now goes through `read_column()`. Add a file here (with a
/// justification) only if it deliberately references the legacy column behind
/// its own `column_exists` guard.
const ALLOWED_FILES: &[&str] = &[];

/// Forbidden alias-qualified references to the legacy chunk embedding column.
const FORBIDDEN: &[&str] = &[
    "c.embedding",
    "ca.embedding",
    "cb.embedding",
    "c2.embedding",
];

#[test]
fn no_sql_references_legacy_chunk_embedding_alias() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("src");
    assert!(src.is_dir(), "src dir missing at {}", src.display());

    let mut files = Vec::new();
    collect_rs(&src, &mut files);

    let mut violations: Vec<String> = Vec::new();
    for path in files {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if ALLOWED_FILES.contains(&name) {
            continue;
        }
        let text = fs::read_to_string(&path).expect("read src file");
        for (i, line) in text.lines().enumerate() {
            // Skip line comments so doc references to the anti-pattern don't
            // trip the guard.
            if line.trim_start().starts_with("//") {
                continue;
            }
            for pat in FORBIDDEN {
                if contains_bare(line, pat) {
                    violations.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "legacy chunk-alias `embedding` SQL found — route through \
         read_active_signature().read_column() (`format!(\"… {{col}} …\")`):\n{}",
        violations.join("\n")
    );
}

/// True only for a "bare" SQL occurrence of `pat`. The char AFTER it must not
/// be a word char (so `c.embedding_v2` / `c.embedding_signature` do NOT match)
/// and must not be `.` — SQL never chains `.` after a column, whereas Rust
/// struct-field reads on a result row (`c.embedding.to_vec()`,
/// `c.embedding.clone()`) do, so this excludes those legitimate Rust accesses.
/// The char BEFORE the alias must not be alphanumeric (so the alias is a clean
/// token, not the tail of a longer identifier).
fn contains_bare(line: &str, pat: &str) -> bool {
    let bytes = line.as_bytes();
    let mut from = 0;
    while let Some(rel) = line[from..].find(pat) {
        let start = from + rel;
        let end = start + pat.len();
        let after_ok = bytes
            .get(end)
            .map(|&b| !b.is_ascii_alphanumeric() && b != b'_' && b != b'.')
            .unwrap_or(true);
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        if after_ok && before_ok {
            return true;
        }
        from = end;
    }
    false
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
