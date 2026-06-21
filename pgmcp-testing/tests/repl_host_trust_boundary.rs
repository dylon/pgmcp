//! Phase 8 trust boundary — the `tape_repl` host glue never writes the corpus,
//! and its admission refusals log at `warn!` (ADR-021 trust-boundary-refused),
//! not `error!`.
//!
//! The white-box REPL is the highest-trust tape capability, so its pgmcp-side
//! wrapper (`src/tape/repl_host.rs`) and tool body (`src/mcp/tools/tool_tape_repl.rs`)
//! must uphold two structural properties:
//!
//!   1. **No corpus write.** The REPL's `put` verb (in context-tape) targets only
//!      tree-local `Scratch`; the durable corpus tables (`file_chunks`,
//!      `indexed_files`, `memory_observations`) are read-only to it. These pgmcp
//!      files must therefore issue NO `INSERT/UPDATE/DELETE` against those tables.
//!   2. **Refusals are `warn!`.** A by-design admission refusal (trust-boundary /
//!      over-limit) is an expected, benign condition — it must log at `warn!`
//!      (visible only at `warn`+), not `error!` (which would imply a swallowed
//!      runtime fault). Parity with `no_swallowed_error_warn.rs`'s convention.
//!
//! This source-grep enforces both at test time, the way `digest_trust_boundary.rs`
//! guards the digest. A future edit that reaches for a corpus write, or downgrades
//! the refusal to `error!`, fails here before it can ship.

use std::path::{Path, PathBuf};

/// Repo root (one level above pgmcp-testing's `CARGO_MANIFEST_DIR`).
fn repo_root() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    Path::new(&manifest)
        .parent()
        .expect("workspace root above pgmcp-testing")
        .to_path_buf()
}

/// The two `tape_repl` host-side source files, as `(label, contents)`.
fn repl_host_sources() -> Vec<(String, String)> {
    let root = repo_root();
    let files = ["src/tape/repl_host.rs", "src/mcp/tools/tool_tape_repl.rs"];
    let mut out = Vec::with_capacity(files.len());
    for rel in files {
        let path = root.join(rel);
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        out.push((rel.to_string(), body));
    }
    out
}

/// Strip `//`-prefixed line comments (incl. `///` doc lines) so a banned token
/// merely *named* in prose (this module and the source files discuss the corpus
/// and `warn!` at length) does not trip the grep. We guard code, not docs.
fn strip_line_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.lines() {
        let code = match line.find("//") {
            Some(idx) => &line[..idx],
            None => line,
        };
        out.push_str(code);
        out.push('\n');
    }
    out
}

#[test]
fn repl_host_writes_no_corpus() {
    // The corpus tables the REPL must never mutate, paired with the mutating SQL
    // verbs that would constitute a write.
    const CORPUS_TABLES: &[&str] = &["file_chunks", "indexed_files", "memory_observations"];
    const MUTATIONS: &[&str] = &["INSERT INTO", "UPDATE", "DELETE FROM"];

    for (label, src) in repl_host_sources() {
        let code = strip_line_comments(&src).to_uppercase();
        for table in CORPUS_TABLES {
            let table_up = table.to_uppercase();
            for verb in MUTATIONS {
                // e.g. "UPDATE FILE_CHUNKS", "INSERT INTO MEMORY_OBSERVATIONS".
                let needle = format!("{verb} {table_up}");
                assert!(
                    !code.contains(&needle),
                    "{label} must not `{verb} {table}` — the tape REPL never writes the \
                     read-only corpus (put is Scratch-only)"
                );
            }
        }
    }
}

#[test]
fn repl_refusal_logs_warn_not_error() {
    // The refusal path lives in `src/tape/repl_host.rs::repl_admitted`. Assert the
    // file emits the refusal at `warn!` (parity with no_swallowed_error_warn.rs).
    let (_label, repl_host) = repl_host_sources()
        .into_iter()
        .find(|(l, _)| l == "src/tape/repl_host.rs")
        .expect("repl_host.rs is in the source set");
    let code = strip_line_comments(&repl_host);

    // The two refusal branches (medium / experiment) each warn!.
    let warn_count = code.matches("tracing::warn!").count();
    assert!(
        warn_count >= 2,
        "repl_admitted must log BOTH refusal branches (trust-boundary medium AND \
         experiment-not-open) at `warn!`; found {warn_count} `tracing::warn!` site(s)"
    );

    // And the refusal path must NOT use error! (an admission refusal is by-design,
    // not a swallowed runtime fault). A genuine DB failure (error!) lives in the
    // tool body, not in the pure gate file — so repl_host.rs carries no error!.
    assert!(
        !code.contains("tracing::error!") && !code.contains("error!("),
        "repl_host.rs must not log a refusal at `error!` — a trust-boundary refusal is \
         an expected, benign condition (ADR-021), so it stays `warn!`"
    );
}
