//! Repo-wide regression test: no `.rs` file under `src/mcp/tools/`
//! may reference `committed_at` (a column that does not exist on
//! `git_commits`). The actual column is `author_date`
//! (`src/db/migrations.rs:245`).
//!
//! On 2026-05-25 a user-facing `semantic_drift` MCP call surfaced the
//! runtime error `column gc.committed_at does not exist`. Investigation
//! found 8 tool files all referencing the nonexistent column — the
//! per-tool regression test at `src/mcp/tools/tool_refactor_pressure.rs:113`
//! had been silently insufficient (it only guarded its own SQL).
//!
//! This test guards every tool file in the tree. If you add a new tool
//! that genuinely needs a column literally named `committed_at` on some
//! other table, the test will tell you to add that file to the
//! `ALLOWED_FILES` set with a justification.

use std::fs;
use std::path::Path;

/// Files allowed to reference `committed_at` because the references
/// are deliberately anti-pattern guards (comments / test names / test
/// assertions that prove the bug is gone).
const ALLOWED_FILES: &[&str] = &["tool_refactor_pressure.rs"];

#[test]
fn no_tool_references_committed_at() {
    let tools_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("src/mcp/tools");
    assert!(
        tools_dir.is_dir(),
        "tools directory missing at {}",
        tools_dir.display()
    );

    let mut violations: Vec<String> = Vec::new();
    for entry in fs::read_dir(&tools_dir).expect("read tools dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        if ALLOWED_FILES.contains(&filename.as_str()) {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                violations.push(format!("could not read {}: {}", path.display(), e));
                continue;
            }
        };
        for (i, line) in content.lines().enumerate() {
            if line.contains("committed_at") {
                violations.push(format!(
                    "{}:{}: contains `committed_at` — git_commits has no \
                     such column (actual column is `author_date`; see \
                     src/db/migrations.rs:245). If you genuinely need a \
                     column literally named `committed_at` on some other \
                     table, add `{}` to ALLOWED_FILES in this test with \
                     a justification.",
                    path.display(),
                    i + 1,
                    filename
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "found `committed_at` references in {} tool file(s):\n{}",
        violations.len(),
        violations.join("\n")
    );
}
