//! Phase 4 trust boundary — the digest is structurally read-only.
//!
//! The proactive digest (`src/digest/`) issues only `SELECT`s for the state it
//! surfaces, plus exactly one INSERT into its own `digest_emissions` rate-limit
//! ledger. It must NEVER drive a status transition or construct a tracker
//! `Actor`. This source-grep test enforces that property the way the roadmap's
//! "unifying invariant" requires: it reads every `src/digest/*.rs` file and
//! asserts the transition symbols are absent. If a future edit reaches for
//! `set_work_item_status` or `Actor::` from the digest, this test fails before
//! the change can ship.
//!
//! Mirrors the source-introspection idiom of `*_cron_registered.rs`.

use std::path::{Path, PathBuf};

/// Repo root (one level above pgmcp-testing's `CARGO_MANIFEST_DIR`).
fn repo_root() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    Path::new(&manifest)
        .parent()
        .expect("workspace root above pgmcp-testing")
        .to_path_buf()
}

/// Every `*.rs` file under `src/digest/`, as `(relative_label, contents)`.
fn digest_sources() -> Vec<(String, String)> {
    let dir = repo_root().join("src").join("digest");
    let mut out = Vec::new();
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            let label = format!(
                "src/digest/{}",
                path.file_name().expect("file name").to_string_lossy()
            );
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            out.push((label, body));
        }
    }
    assert!(
        !out.is_empty(),
        "expected at least one src/digest/*.rs file (mod.rs, webhook.rs)"
    );
    out
}

/// Strip `//`-prefixed line comments (incl. `///` doc lines) so a banned token
/// merely *named* in prose — like this very module's trust-note comments —
/// doesn't trip the grep. We are guarding code, not documentation.
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
fn digest_never_transitions_status() {
    for (label, src) in digest_sources() {
        let code = strip_line_comments(&src);
        assert!(
            !code.contains("set_work_item_status"),
            "{label} must not call set_work_item_status — the digest is read-only \
             (issues only SELECTs + its own digest_emissions insert)"
        );
    }
}

#[test]
fn digest_never_constructs_an_actor() {
    for (label, src) in digest_sources() {
        let code = strip_line_comments(&src);
        assert!(
            !code.contains("Actor::"),
            "{label} must not reference Actor:: — the digest performs no \
             tracker transitions of any actor kind"
        );
    }
}

#[test]
fn digest_writes_only_its_own_ledger() {
    // Defense-in-depth: the only mutating SQL verb the digest may issue is the
    // INSERT into digest_emissions. No UPDATE/DELETE against work_items (or any
    // status column). This catches a raw `UPDATE work_items SET status …` that
    // bypassed the `set_work_item_status` chokepoint.
    for (label, src) in digest_sources() {
        let code = strip_line_comments(&src).to_uppercase();
        assert!(
            !code.contains("UPDATE WORK_ITEMS"),
            "{label} must not UPDATE work_items"
        );
        assert!(
            !code.contains("DELETE FROM WORK_ITEMS"),
            "{label} must not DELETE FROM work_items"
        );
    }
}
