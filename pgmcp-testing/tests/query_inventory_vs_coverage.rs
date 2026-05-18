//! Layer D of the integration-test plan: a static safety net that fails
//! at test time if a new MCP tool is added to the `call_tool_cli`
//! dispatch table without a corresponding integration test.
//!
//! The orient regression (commit 802ca00 → fixed in this PR) shipped
//! because nothing in CI checked that every dispatched tool had at
//! least one test that actually executed its SQL. This test closes
//! that gap.
//!
//! ## How it works
//!
//! 1. Read `src/mcp/server.rs` and extract every dispatch entry
//!    (`"<name>" => …`) from inside the `dispatch_tool!` block in
//!    `call_tool_cli`.
//! 2. Read every `*.rs` file under `pgmcp-testing/tests/` and extract
//!    every `call_tool_cli("<name>"` invocation.
//! 3. Diff. If a dispatched tool has no corresponding `call_tool_cli`
//!    invocation in any test file, fail with a diff of missing names.
//!
//! ## How to fix a failure
//!
//! If you added a new tool and this test fails:
//!   - Add `<your_tool>` to the dispatch table in
//!     `src/mcp/server.rs::call_tool_cli` (or you already did, that's
//!     why you're here).
//!   - Add at least one `#[tokio::test]` in `pgmcp-testing/tests/`
//!     that calls `server.call_tool_cli("<your_tool>", json!(…))` and
//!     asserts `Ok`. The minimal pattern is in
//!     `query_smoke_mcp_tools.rs`.
//!
//! Note: this test reads source files directly off disk. It will run
//! identically in CI, on developer machines, and via `cargo test`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR for the integration tests in pgmcp-testing is
    // `<repo>/pgmcp-testing`. Walk up one to get the repo root.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("repo root above pgmcp-testing")
        .to_path_buf()
}

fn read_text(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e))
}

/// Extract dispatched tool names from `call_tool_cli` in `src/mcp/server.rs`.
///
/// The dispatch entries look like:
///   `"tool_name" => method(ParamsTy),`
///   `"tool_name" => method(ParamsTy) in body_mod,`
///   `"tool_name" => method,`  (no_params variant)
///
/// We capture everything inside the `dispatch_tool!(…)` block that
/// contains both `"<word>" =>` patterns. We don't try to parse Rust;
/// regex over the lines is sufficient because the dispatch macro has a
/// fixed shape.
fn extract_dispatched_tools(server_src: &str) -> BTreeSet<String> {
    // Find the start of `dispatch_tool!(`. The macro has the shape
    //   dispatch_tool!(self, name, args, { … }, no_params: { … })
    // so we must balance BOTH parens and braces — stopping at the
    // first `}` only catches the params block and misses no_params.
    // Look for the macro CALL site, not the rustdoc or macro_rules!
    // definition. The call is the only place `dispatch_tool!(` appears.
    let macro_kw = "dispatch_tool!(";
    let macro_start = server_src
        .find(macro_kw)
        .expect("dispatch_tool!( must exist in server.rs (call site)");
    let paren_start = macro_start + macro_kw.len() - 1; // index of the '('

    let bytes = server_src.as_bytes();
    let mut depth: i32 = 0;
    let mut end = paren_start;
    for (i, &b) in bytes.iter().enumerate().skip(paren_start) {
        match b {
            b'(' | b'{' => depth += 1,
            b')' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    let block = &server_src[paren_start..end];

    // Extract every `"<word>" =>` literal. The macro form mandates
    // a string literal on the LHS so this is unambiguous.
    let mut tools = BTreeSet::new();
    for line in block.lines() {
        let trimmed = line.trim();
        // Skip comments. The dispatch block has many `//` annotations.
        if trimmed.starts_with("//") {
            continue;
        }
        if let Some((lit, _)) = trimmed.split_once("=>") {
            let lit = lit.trim();
            if let Some(name) = lit.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                if !name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
                    tools.insert(name.to_string());
                }
            }
        }
    }
    tools
}

/// Extract every tool name passed to `server.call_tool_cli("<name>", …)`
/// across all `*.rs` files under `pgmcp-testing/tests/`.
fn extract_tested_tools(tests_dir: &Path) -> BTreeSet<String> {
    let mut covered = BTreeSet::new();
    walk_rs(tests_dir, &mut |path| {
        let src = read_text(path);
        for chunk in src.split("call_tool_cli(").skip(1) {
            // Next token should be `"<name>"`.
            let chunk = chunk.trim_start();
            if let Some(rest) = chunk.strip_prefix('"') {
                if let Some(end) = rest.find('"') {
                    let name = &rest[..end];
                    if !name.is_empty() {
                        covered.insert(name.to_string());
                    }
                }
            }
        }
    });
    covered
}

fn walk_rs(dir: &Path, f: &mut dyn FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_rs(&path, f);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            f(&path);
        }
    }
}

#[test]
fn every_dispatched_tool_has_an_integration_test() {
    let root = repo_root();
    let server_src = read_text(&root.join("src/mcp/server.rs"));
    let dispatched = extract_dispatched_tools(&server_src);
    let covered = extract_tested_tools(&root.join("pgmcp-testing/tests"));

    assert!(
        !dispatched.is_empty(),
        "extracted zero dispatched tools — the regex must have drifted from server.rs"
    );

    let missing: Vec<&String> = dispatched.difference(&covered).collect();

    if !missing.is_empty() {
        let mut msg = String::new();
        msg.push_str(&format!(
            "\n{} tool(s) in call_tool_cli have NO corresponding test:\n",
            missing.len()
        ));
        for name in &missing {
            msg.push_str(&format!("  - {}\n", name));
        }
        msg.push_str(
            "\nTo fix: add a #[tokio::test] in pgmcp-testing/tests/ that calls\n\
             `server.call_tool_cli(\"<name>\", …)` and asserts Ok. The minimal\n\
             pattern is in query_smoke_mcp_tools.rs.\n\
             \n\
             This safety net is documented in\n\
             /home/dylon/.claude/plans/identify-the-root-cause-functional-wren.md\n\
             (Layer D).",
        );
        panic!("{}", msg);
    }
}

#[test]
fn extract_dispatched_tools_finds_known_anchors() {
    // Sanity check that the regex hasn't drifted. orient must be
    // present (we added it explicitly); semantic_search must be
    // present (one of the oldest entries); list_projects must be
    // present (no_params variant).
    let root = repo_root();
    let server_src = read_text(&root.join("src/mcp/server.rs"));
    let dispatched = extract_dispatched_tools(&server_src);

    for anchor in &["orient", "semantic_search", "list_projects"] {
        assert!(
            dispatched.contains(*anchor),
            "anchor tool {} not found in extracted dispatch list — \
             extract_dispatched_tools regex has drifted",
            anchor
        );
    }
}
