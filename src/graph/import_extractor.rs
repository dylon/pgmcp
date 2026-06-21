//! Regex-based import extraction for multiple programming languages.
//! Extracts import/dependency relationships from source code content.
//!
//! As of Tier-0e (tree-sitter parsing layer, see `src/parsing/`), this module
//! is the **fallback path** for `src/cron/graph_analysis.rs::analyze_project`.
//! Files with rows in `symbol_references` (populated by the
//! `symbol-extraction` cron) are sourced from the `import_use` rows directly;
//! files without such rows fall back to the regex extractors here. The
//! resolver helpers (`resolve_import_candidates`, etc.) are still consumed
//! by the symbol-aware path for `target_file_id` lookup, since the
//! symbol-extraction cron resolves only by name match — not by per-language
//! file-path conventions.

use regex::Regex;
use std::sync::LazyLock;

use crate::graph::cargo_layout::CrateLayout;

/// A raw import extracted from source code.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RawImport {
    /// The raw import path as written in source (e.g., "crate::db::queries").
    pub raw_path: String,
    /// The import kind (e.g., "use", "mod", "import", "require", "include").
    pub kind: String,
}

// ============================================================================
// Language-specific regex patterns
// ============================================================================

static RUST_USE: LazyLock<Regex> = LazyLock::new(|| {
    // Capture any identifier-rooted `use` path, not just `crate`/`super`/`self`.
    // The leading segment of a cross-crate `use` is the crate's library
    // identifier (`mettail_prattail`), which the resolver maps to a workspace
    // member's source dir via `CrateLayout`. External crates (`std`, `tokio`)
    // are captured too but resolve to no candidate (left unresolved, as before).
    // Grouped/`as`/glob tails self-heal: `(?:::\w+)+` stops at `{`/`*`/` as `,
    // yielding the module-path prefix which `rust_path_candidates` resolves.
    Regex::new(r"(?m)^\s*use\s+(\w+(?:::\w+)+)").expect("invalid regex")
});
static RUST_MOD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*(?:pub\s+)?mod\s+(\w+)\s*;").expect("invalid regex"));
static RUST_EXTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*extern\s+crate\s+(\w+)").expect("invalid regex"));

static PYTHON_IMPORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*import\s+([\w.]+)").expect("invalid regex"));
static PYTHON_FROM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*from\s+([\w.]+)\s+import").expect("invalid regex"));

static JS_IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*import\s+(?:.*?\s+from\s+)?['"]([^'"]+)['"]"#).expect("invalid regex")
});
static JS_REQUIRE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)require\(\s*['"]([^'"]+)['"]\s*\)"#).expect("invalid regex")
});
static JS_EXPORT_FROM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*export\s+.*?\s+from\s+['"]([^'"]+)['"]"#).expect("invalid regex")
});

static GO_IMPORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)"([\w./-]+)""#).expect("invalid regex"));

static JAVA_IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*import\s+(?:static\s+)?([\w.]+)").expect("invalid regex")
});

static C_INCLUDE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*#\s*include\s*[<"]([\w/._-]+)[>"]"#).expect("invalid regex")
});

/// Clojure namespace tokens. We don't parse the `ns` form structurally here
/// (that's the tree-sitter backend's job in `src/parsing/clojure.rs`); the
/// fallback simply harvests every bracketed/quoted namespace symbol inside a
/// `:require` / `:use` / `:import` clause or a top-level `(require '…)`. A
/// Clojure namespace symbol is dotted, may contain hyphens, and ends each
/// segment in an identifier char. We match the leading symbol of a vector spec
/// (`[a.b.c :as x]`), a bare symbol (`a.b.c`), or a quoted symbol (`'a.b.c`).
static CLOJURE_NS_SYMBOL: LazyLock<Regex> = LazyLock::new(|| {
    // A namespace symbol: starts with a letter, contains alnum / `.` / `-` /
    // `_` / `*` / `?` / `!`, and has at least one `.` OR stands alone. We keep
    // it permissive but anchor on the contexts via CLOJURE_REQUIRE_BLOCK.
    Regex::new(r"[A-Za-z][A-Za-z0-9_.*?!+-]*").expect("invalid regex")
});
/// Matches a `(:require …)`, `(:use …)`, `(:import …)` clause body, or a
/// top-level `(require …)` / `(use …)` / `(import …)` form, capturing the body
/// up to the matching paren depth heuristically (a single non-nested level,
/// which covers the common `ns` layout).
static CLOJURE_REQUIRE_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)\(\s*:?(?:require|use|import)\b([^()]*(?:\([^()]*\)[^()]*)*)")
        .expect("invalid regex")
});

/// Extract all imports from source content for a given language.
pub fn extract_imports(content: &str, language: &str) -> Vec<RawImport> {
    match language {
        "rust" => extract_rust_imports(content),
        "python" => extract_python_imports(content),
        "javascript" | "typescript" => extract_js_imports(content),
        "go" => extract_go_imports(content),
        "java" | "kotlin" => extract_java_imports(content),
        "c" | "cpp" | "c++" | "header" => extract_c_imports(content),
        "clojure" | "clojurescript" => extract_clojure_imports(content),
        _ => Vec::new(),
    }
}

fn extract_rust_imports(content: &str) -> Vec<RawImport> {
    let mut imports = Vec::new();

    for cap in RUST_USE.captures_iter(content) {
        let m = cap.get(1).expect("capture group 1 must exist");
        imports.push(RawImport {
            raw_path: m.as_str().to_string(),
            kind: "use".to_string(),
        });
    }

    for cap in RUST_MOD.captures_iter(content) {
        let m = cap.get(1).expect("capture group 1 must exist");
        imports.push(RawImport {
            raw_path: m.as_str().to_string(),
            kind: "mod".to_string(),
        });
    }

    for cap in RUST_EXTERN.captures_iter(content) {
        let m = cap.get(1).expect("capture group 1 must exist");
        imports.push(RawImport {
            raw_path: m.as_str().to_string(),
            kind: "extern_crate".to_string(),
        });
    }

    imports
}

fn extract_python_imports(content: &str) -> Vec<RawImport> {
    let mut imports = Vec::new();

    for cap in PYTHON_IMPORT.captures_iter(content) {
        let m = cap.get(1).expect("capture group 1 must exist");
        imports.push(RawImport {
            raw_path: m.as_str().to_string(),
            kind: "import".to_string(),
        });
    }

    for cap in PYTHON_FROM.captures_iter(content) {
        let m = cap.get(1).expect("capture group 1 must exist");
        imports.push(RawImport {
            raw_path: m.as_str().to_string(),
            kind: "from".to_string(),
        });
    }

    imports
}

fn extract_js_imports(content: &str) -> Vec<RawImport> {
    let mut imports = Vec::new();

    for cap in JS_IMPORT.captures_iter(content) {
        let m = cap.get(1).expect("capture group 1 must exist");
        imports.push(RawImport {
            raw_path: m.as_str().to_string(),
            kind: "import".to_string(),
        });
    }

    for cap in JS_REQUIRE.captures_iter(content) {
        let m = cap.get(1).expect("capture group 1 must exist");
        imports.push(RawImport {
            raw_path: m.as_str().to_string(),
            kind: "require".to_string(),
        });
    }

    for cap in JS_EXPORT_FROM.captures_iter(content) {
        let m = cap.get(1).expect("capture group 1 must exist");
        imports.push(RawImport {
            raw_path: m.as_str().to_string(),
            kind: "export_from".to_string(),
        });
    }

    imports
}

fn extract_go_imports(content: &str) -> Vec<RawImport> {
    let mut imports = Vec::new();

    for cap in GO_IMPORT.captures_iter(content) {
        let m = cap.get(1).expect("capture group 1 must exist");
        imports.push(RawImport {
            raw_path: m.as_str().to_string(),
            kind: "import".to_string(),
        });
    }

    imports
}

fn extract_java_imports(content: &str) -> Vec<RawImport> {
    let mut imports = Vec::new();

    for cap in JAVA_IMPORT.captures_iter(content) {
        let m = cap.get(1).expect("capture group 1 must exist");
        imports.push(RawImport {
            raw_path: m.as_str().to_string(),
            kind: "import".to_string(),
        });
    }

    imports
}

fn extract_c_imports(content: &str) -> Vec<RawImport> {
    let mut imports = Vec::new();

    for cap in C_INCLUDE.captures_iter(content) {
        let m = cap.get(1).expect("capture group 1 must exist");
        imports.push(RawImport {
            raw_path: m.as_str().to_string(),
            kind: "include".to_string(),
        });
    }

    imports
}

/// Clojure / ClojureScript keywords that appear inside a `:require` / `:import`
/// vector and must NOT be treated as namespace symbols.
const CLOJURE_SPEC_KEYWORDS: &[&str] = &[
    ":as",
    ":as-alias",
    ":refer",
    ":refer-macros",
    ":rename",
    ":only",
    ":exclude",
    ":include-macros",
    ":reload",
    ":reload-all",
    ":verbose",
    "true",
    "false",
    "nil",
];

fn extract_clojure_imports(content: &str) -> Vec<RawImport> {
    let mut imports = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for block in CLOJURE_REQUIRE_BLOCK.captures_iter(content) {
        let body = match block.get(1) {
            Some(m) => m.as_str(),
            None => continue,
        };
        // The first symbol of each vector spec (or a bare/quoted symbol) is the
        // namespace. We harvest every namespace-looking token, then filter out
        // the spec keywords (`:as`, …) and the `:refer`'d names that follow a
        // vector. A `:refer [a b]` list's `a`/`b` are var names, not namespaces;
        // they live inside a nested `[...]` after `:refer`, which our token
        // scanner would otherwise pick up. We drop tokens appearing after a
        // `:refer` / `:only` / `:rename` keyword within the same spec by
        // splitting on those markers.
        for spec in split_clojure_specs(body) {
            // Within one spec, take only the leading symbol (before any keyword).
            let head = spec
                .split_whitespace()
                .find(|t| !t.is_empty())
                .unwrap_or("");
            // Strip a leading quote / vector bracket and a trailing bracket.
            let cleaned = head
                .trim_start_matches('\'')
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim_matches('"');
            if cleaned.is_empty() {
                continue;
            }
            // Validate it's a namespace symbol and not a keyword/number.
            if CLOJURE_SPEC_KEYWORDS.contains(&cleaned)
                || cleaned.starts_with(':')
                || !CLOJURE_NS_SYMBOL.is_match(cleaned)
            {
                continue;
            }
            // A pure number or single punctuation is not a namespace.
            if cleaned.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if seen.insert(cleaned.to_string()) {
                imports.push(RawImport {
                    raw_path: cleaned.to_string(),
                    kind: "require".to_string(),
                });
            }
        }
    }

    imports
}

/// Split a `:require`/`:import` clause body into individual specs. Each spec is
/// either a bracketed vector `[ns …]`, a quoted symbol `'ns`, or a bare symbol
/// `ns`. We tokenize by splitting on the vector brackets while keeping the
/// leading symbol of each bracketed group, plus the bare symbols between them.
fn split_clojure_specs(body: &str) -> Vec<String> {
    let mut specs: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    for ch in body.chars() {
        match ch {
            '[' | '(' => {
                if depth == 0 {
                    // Flush any bare symbols accumulated at top level.
                    flush_bare_specs(&current, &mut specs);
                    current.clear();
                }
                depth += 1;
            }
            ']' | ')' => {
                depth -= 1;
                if depth == 0 {
                    // `current` holds the inside of a vector spec.
                    specs.push(current.trim().to_string());
                    current.clear();
                }
            }
            _ => {
                current.push(ch);
            }
        }
    }
    if depth == 0 && !current.trim().is_empty() {
        flush_bare_specs(&current, &mut specs);
    }
    specs
}

/// Treat each whitespace-separated bare token (outside any bracket) as its own
/// single-symbol spec, so `(:require a.b c.d)` yields two specs.
fn flush_bare_specs(chunk: &str, specs: &mut Vec<String>) {
    for tok in chunk.split_whitespace() {
        let t = tok.trim_start_matches('\'');
        if !t.is_empty() && !t.starts_with(':') {
            specs.push(t.to_string());
        }
    }
}

// ============================================================================
// Import resolution
// ============================================================================

/// Resolve raw imports to candidate relative file paths within a project.
/// `source_relative_path`: the file containing the import (e.g., "src/mcp/server.rs").
/// `layout`: the project's Cargo crate layout (`Some` for Rust workspaces), used
/// to resolve cross-crate `use <ident>::…` to the owning member's source dir.
/// `None` preserves the legacy `crate::`/`super::`-only behavior.
pub fn resolve_import_candidates(
    import: &RawImport,
    source_relative_path: &str,
    language: &str,
    layout: Option<&CrateLayout>,
) -> Vec<String> {
    match language {
        "rust" => resolve_rust_import(import, source_relative_path, layout),
        "python" => resolve_python_import(import, source_relative_path),
        "javascript" | "typescript" => resolve_js_import(import, source_relative_path),
        "clojure" | "clojurescript" => resolve_clojure_import(import, source_relative_path),
        _ => Vec::new(),
    }
}

fn resolve_rust_import(
    import: &RawImport,
    source_relative_path: &str,
    layout: Option<&CrateLayout>,
) -> Vec<String> {
    let path = &import.raw_path;

    match &*import.kind {
        "use" => {
            if let Some(rest) = path.strip_prefix("crate::") {
                // `crate::` resolves relative to THIS file's crate root. For a
                // single-crate project that is the top-level "src"; for a cargo
                // workspace member the file lives under "<member>/src/...", so we
                // derive the crate's src dir from the source path rather than
                // hardcoding "src" (which left every workspace member's `crate::`
                // imports unresolvable → empty import graph for the whole repo).
                // e.g. crate::db::queries -> <crate_src>/db/queries.rs|/mod.rs
                let base = rust_crate_src_root(source_relative_path);
                let segments: Vec<&str> = rest.split("::").collect();
                rust_path_candidates(&base, &segments)
            } else if let Some(rest) = path.strip_prefix("super::") {
                // super::foo -> sibling module relative to current
                let parent = parent_module(source_relative_path);
                let segments: Vec<&str> = rest.split("::").collect();
                rust_path_candidates(&parent, &segments)
            } else if let Some(rest) = path.strip_prefix("self::") {
                // self::foo -> a child module of the CURRENT module. For a file
                // `a/b.rs` the current module's children live under `a/b/`; for
                // `a/mod.rs` (or `a/b/mod.rs`) they live in the same directory.
                let base = rust_self_module_root(source_relative_path);
                let segments: Vec<&str> = rest.split("::").collect();
                rust_path_candidates(&base, &segments)
            } else if let Some((ident, rest)) = path.split_once("::") {
                // Cross-crate `use <ident>::path::Item;`. Map the crate library
                // identifier to its source dir via the workspace Cargo layout;
                // an unknown ident (std/tokio/3rd-party, or no layout) stays
                // unresolved — exactly the prior behavior for externals.
                match layout.and_then(|l| l.src_dir_for(ident)) {
                    Some(src_dir) => {
                        let segments: Vec<&str> = rest.split("::").collect();
                        rust_path_candidates(src_dir, &segments)
                    }
                    None => Vec::new(),
                }
            } else {
                // Bare single-segment `use foo;` — no module path to resolve.
                Vec::new()
            }
        }
        "mod" => {
            // mod foo; -> look for foo.rs or foo/mod.rs in same directory
            let dir = parent_dir(source_relative_path);
            let mod_name = path.as_str();
            vec![
                format!("{}/{}.rs", dir, mod_name),
                format!("{}/{}/mod.rs", dir, mod_name),
            ]
        }
        _ => Vec::new(),
    }
}

fn resolve_python_import(import: &RawImport, source_relative_path: &str) -> Vec<String> {
    let path = &import.raw_path;

    if path.starts_with('.') {
        // Relative import
        let dir = parent_dir(source_relative_path);
        let rest = path.trim_start_matches('.');
        let segments: Vec<&str> = rest.split('.').filter(|s| !s.is_empty()).collect();
        let base = if segments.is_empty() {
            dir.to_string()
        } else {
            format!("{}/{}", dir, segments.join("/"))
        };
        vec![format!("{}.py", base), format!("{}/__init__.py", base)]
    } else {
        // Absolute import: try as package path
        let segments = path.replace('.', "/");
        vec![
            format!("{}.py", segments),
            format!("{}/__init__.py", segments),
        ]
    }
}

fn resolve_js_import(import: &RawImport, source_relative_path: &str) -> Vec<String> {
    let path = &import.raw_path;

    if path.starts_with('.') || path.starts_with('/') {
        // Relative import
        let dir = parent_dir(source_relative_path);
        let resolved = normalize_path(&format!("{}/{}", dir, path));
        vec![
            format!("{}.js", resolved),
            format!("{}.ts", resolved),
            format!("{}.jsx", resolved),
            format!("{}.tsx", resolved),
            format!("{}/index.js", resolved),
            format!("{}/index.ts", resolved),
            resolved, // exact match
        ]
    } else {
        // External package
        Vec::new()
    }
}

/// Resolve a Clojure namespace symbol (`a.b.c`) to candidate source files.
///
/// Clojure maps namespace dots to directory separators and munges hyphens in
/// the namespace into underscores in the file path (`my-app.core` →
/// `my_app/core.clj`). We derive the classpath source root from the source
/// file's own path (the FIRST `src` component, mirroring `rust_crate_src_root`)
/// so multi-module / Leiningen-profile layouts resolve, and emit `.clj`,
/// `.cljs`, and `.cljc` candidates. Java-class `:import` targets (containing an
/// uppercase class segment) are skipped — they resolve to the JDK / jars, not
/// project files.
fn resolve_clojure_import(import: &RawImport, source_relative_path: &str) -> Vec<String> {
    let ns = import.raw_path.trim();
    if ns.is_empty() {
        return Vec::new();
    }
    // CLJS string-module requires (e.g. `"react"`) and Java packages are not
    // project files; a Java import like `java.util.Date` has an uppercase final
    // segment. Skip those — they're external.
    if ns.contains('/') {
        // Munged already or a JS module path with slashes — not our convention.
        return Vec::new();
    }
    let segments: Vec<String> = ns.split('.').map(|seg| seg.replace('-', "_")).collect();
    if segments.is_empty() {
        return Vec::new();
    }
    // Heuristic: a trailing PascalCase segment indicates a Java class import
    // (`java.util.Date`), which is not a project namespace file.
    if let Some(last) = ns.rsplit('.').next()
        && last.chars().next().is_some_and(|c| c.is_ascii_uppercase())
    {
        return Vec::new();
    }
    let src_root = clojure_src_root(source_relative_path);
    let rel = segments.join("/");
    let base = if src_root.is_empty() {
        rel
    } else {
        format!("{}/{}", src_root, rel)
    };
    vec![
        format!("{}.clj", base),
        format!("{}.cljs", base),
        format!("{}.cljc", base),
    ]
}

/// Derive the classpath source root for a Clojure file. Uses the first `src`
/// path component (so `modules/foo/src/bar/core.clj` → `modules/foo/src`);
/// falls back to `"src"`.
fn clojure_src_root(source_relative_path: &str) -> String {
    let parts: Vec<&str> = source_relative_path.split('/').collect();
    match parts.iter().position(|&p| p == "src") {
        Some(idx) => parts[..=idx].join("/"),
        None => "src".to_string(),
    }
}

fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(pos) => &path[..pos],
        None => "",
    }
}

fn parent_module(path: &str) -> String {
    let dir = parent_dir(path);
    // Go up one more level for super::
    parent_dir(dir).to_string()
}

/// Derive the crate `src` root from a project-relative source path so that
/// `crate::` imports resolve correctly in multi-crate cargo workspaces. For a
/// single crate at the project root this is `"src"`; for a workspace member at
/// e.g. `crates/foo/src/bar.rs` it is `crates/foo/src`. Uses the FIRST `src`
/// path component (the outermost crate boundary from the project root) and
/// falls back to `"src"` when no `src` component is present.
fn rust_crate_src_root(source_relative_path: &str) -> String {
    let parts: Vec<&str> = source_relative_path.split('/').collect();
    match parts.iter().position(|&p| p == "src") {
        Some(idx) => parts[..=idx].join("/"),
        None => "src".to_string(),
    }
}

/// Directory under which a file's `self::` child modules live. For `a/b.rs`
/// the current module is `a::b`, whose children sit in `a/b/`; for a module
/// root file (`a/mod.rs`, `lib.rs`, `main.rs`) the children sit in the same
/// directory.
fn rust_self_module_root(source_relative_path: &str) -> String {
    let dir = parent_dir(source_relative_path);
    let stem = source_relative_path
        .rsplit('/')
        .next()
        .and_then(|f| f.strip_suffix(".rs"))
        .unwrap_or("");
    if matches!(stem, "mod" | "lib" | "main" | "") {
        dir.to_string()
    } else if dir.is_empty() {
        stem.to_string()
    } else {
        format!("{dir}/{stem}")
    }
}

/// Generate candidate relative file paths for a module path rooted at `base`.
/// Tries progressively shorter module prefixes (the trailing segments may name
/// an item within the module, not a submodule) and both `<mod>.rs` and
/// `<mod>/mod.rs` layouts. `pub` so the cross-project resolver
/// (`crate::graph::workspace_crate_map`) can reuse the exact same convention.
pub fn rust_path_candidates(base: &str, segments: &[&str]) -> Vec<String> {
    if segments.is_empty() {
        return Vec::new();
    }

    // Take just the module path segments (not the item within the module)
    // For "crate::db::queries::foo", we want src/db/queries.rs
    // We try progressively shorter paths since the last segments might be items
    let mut candidates = Vec::new();
    for take in (1..=segments.len()).rev() {
        let module_path = segments[..take].join("/");
        candidates.push(format!("{}/{}.rs", base, module_path));
        candidates.push(format!("{}/{}/mod.rs", base, module_path));
    }
    candidates
}

fn normalize_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn rust_use_crate_path_is_captured() {
        let src = "use crate::db::queries;\nfn main() {}";
        let imports = extract_imports(src, "rust");
        assert!(imports.iter().any(|i| i.raw_path == "crate::db::queries"));
    }

    #[test]
    fn rust_mod_declaration_is_captured() {
        let src = "mod foo;\npub mod bar;\n";
        let imports = extract_imports(src, "rust");
        assert_eq!(imports.iter().filter(|i| i.kind == "mod").count(), 2);
    }

    #[test]
    fn rust_extern_crate_is_captured() {
        let src = "extern crate serde;\n";
        let imports = extract_imports(src, "rust");
        assert!(
            imports
                .iter()
                .any(|i| i.kind == "extern_crate" && i.raw_path == "serde")
        );
    }

    #[test]
    fn crate_src_root_single_crate_is_src() {
        // Single-crate project: file directly under top-level src/.
        assert_eq!(rust_crate_src_root("src/db/queries.rs"), "src");
        assert_eq!(rust_crate_src_root("src/main.rs"), "src");
    }

    #[test]
    fn crate_src_root_workspace_member_uses_member_src() {
        // Cargo workspace member: crate::-imports must resolve under the
        // member's own src/, not the repo-root src/.
        assert_eq!(
            rust_crate_src_root("crates/foo/src/bar.rs"),
            "crates/foo/src"
        );
        assert_eq!(rust_crate_src_root("libs/x/src/a/b.rs"), "libs/x/src");
    }

    #[test]
    fn crate_src_root_no_src_component_falls_back() {
        assert_eq!(rust_crate_src_root("build.rs"), "src");
        assert_eq!(rust_crate_src_root("examples/demo.rs"), "src");
    }

    #[test]
    fn crate_use_resolves_under_workspace_member_src() {
        // End-to-end: a `use crate::a::b;` in a workspace member resolves to
        // candidate paths rooted at the member's src dir.
        let imp = RawImport {
            raw_path: "crate::a::b".to_string(),
            kind: "use".to_string(),
        };
        let candidates = resolve_rust_import(&imp, "crates/foo/src/lib.rs", None);
        assert!(
            candidates.contains(&"crates/foo/src/a/b.rs".to_string()),
            "expected member-rooted candidate, got {candidates:?}"
        );
    }

    #[test]
    fn cross_crate_use_resolves_via_layout_when_ident_ne_directory() {
        // Directory `prattail/` exposes lib ident `mettail_prattail`. A
        // cross-crate `use mettail_prattail::wpda::Foo;` must resolve under the
        // member's src dir even though ident != directory.
        let layout = CrateLayout::from_map(
            [("mettail_prattail".to_string(), "prattail/src".to_string())]
                .into_iter()
                .collect(),
        );
        let imp = RawImport {
            raw_path: "mettail_prattail::wpda::Foo".to_string(),
            kind: "use".to_string(),
        };
        let cands = resolve_rust_import(&imp, "runtime/src/lib.rs", Some(&layout));
        assert!(
            cands.contains(&"prattail/src/wpda.rs".to_string())
                && cands.contains(&"prattail/src/wpda/mod.rs".to_string()),
            "expected wpda module candidates under prattail/src, got {cands:?}"
        );
    }

    #[test]
    fn cross_crate_glob_resolves_via_shorter_prefix_fallback() {
        let layout = CrateLayout::from_map(
            [("mettail_prattail".to_string(), "prattail/src".to_string())]
                .into_iter()
                .collect(),
        );
        let imp = RawImport {
            raw_path: "mettail_prattail::wpda::*".to_string(),
            kind: "use".to_string(),
        };
        let cands = resolve_rust_import(&imp, "runtime/src/lib.rs", Some(&layout));
        assert!(
            cands.contains(&"prattail/src/wpda.rs".to_string()),
            "glob tail should self-heal to the module file, got {cands:?}"
        );
    }

    #[test]
    fn external_crate_unknown_ident_stays_unresolved() {
        let layout = CrateLayout::from_map(
            [("mettail_prattail".to_string(), "prattail/src".to_string())]
                .into_iter()
                .collect(),
        );
        // std/tokio/etc. are not workspace members → no candidates.
        let imp = RawImport {
            raw_path: "std::collections::HashMap".to_string(),
            kind: "use".to_string(),
        };
        assert!(resolve_rust_import(&imp, "runtime/src/lib.rs", Some(&layout)).is_empty());
        // And with no layout at all, externals also stay unresolved (back-compat).
        assert!(resolve_rust_import(&imp, "runtime/src/lib.rs", None).is_empty());
    }

    #[test]
    fn self_module_resolves_under_current_module_dir() {
        // In `a/b.rs`, `self::c` is the child module `a::b::c` under `a/b/`.
        let imp = RawImport {
            raw_path: "self::c".to_string(),
            kind: "use".to_string(),
        };
        let cands = resolve_rust_import(&imp, "a/b.rs", None);
        assert!(
            cands.contains(&"a/b/c.rs".to_string()),
            "self:: child should resolve under the module dir, got {cands:?}"
        );
        // In a module root (`a/mod.rs`), `self::c` is a sibling `a/c.rs`.
        let cands_root = resolve_rust_import(&imp, "a/mod.rs", None);
        assert!(
            cands_root.contains(&"a/c.rs".to_string()),
            "self:: from mod.rs resolves in the same dir, got {cands_root:?}"
        );
    }

    #[test]
    fn widened_rust_use_regex_captures_external_crate_root() {
        // The regex fallback now harvests any ident-rooted `use`, so the
        // symbol-less regex path can also resolve workspace-member crates.
        let src = "use mettail_prattail::wpda::Foo;\nuse crate::db::queries;\n";
        let imports = extract_imports(src, "rust");
        let paths: Vec<&str> = imports.iter().map(|i| i.raw_path.as_str()).collect();
        assert!(paths.contains(&"mettail_prattail::wpda::Foo"));
        assert!(paths.contains(&"crate::db::queries"));
    }

    #[test]
    fn python_import_and_from_captured() {
        let src = "import os\nfrom pathlib import Path\n";
        let imports = extract_imports(src, "python");
        let paths: Vec<&str> = imports.iter().map(|i| i.raw_path.as_str()).collect();
        assert!(paths.contains(&"os"));
        assert!(paths.contains(&"pathlib"));
    }

    #[test]
    fn javascript_import_from_quoted_path() {
        let src = "import { foo } from 'lodash';\nconst x = require(\"fs\");\n";
        let imports = extract_imports(src, "javascript");
        let paths: Vec<&str> = imports.iter().map(|i| i.raw_path.as_str()).collect();
        assert!(paths.contains(&"lodash"));
        assert!(paths.contains(&"fs"));
    }

    #[test]
    fn typescript_uses_same_extractor_as_js() {
        let src = "import { Foo } from './bar';";
        assert_eq!(
            extract_imports(src, "typescript").len(),
            extract_imports(src, "javascript").len(),
        );
    }

    #[test]
    fn java_import_captures_package() {
        let src =
            "package com.example;\nimport java.util.List;\nimport static java.lang.Math.PI;\n";
        let imports = extract_imports(src, "java");
        let paths: Vec<&str> = imports.iter().map(|i| i.raw_path.as_str()).collect();
        assert!(paths.contains(&"java.util.List"));
        assert!(paths.contains(&"java.lang.Math.PI"));
    }

    #[test]
    fn c_include_brackets_and_quotes() {
        let src = "#include <stdio.h>\n#include \"local.h\"\n";
        let imports = extract_imports(src, "c");
        let paths: Vec<&str> = imports.iter().map(|i| i.raw_path.as_str()).collect();
        assert!(paths.contains(&"stdio.h"));
        assert!(paths.contains(&"local.h"));
    }

    #[test]
    fn clojure_ns_require_captures_namespaces() {
        let src = r#"
(ns my.app
  (:require [clojure.string :as str :refer [join]]
            [clojure.set]
            ["react" :as react])
  (:import java.util.Date))
"#;
        let imports = extract_imports(src, "clojure");
        let paths: Vec<&str> = imports.iter().map(|i| i.raw_path.as_str()).collect();
        assert!(paths.contains(&"clojure.string"), "imports: {:?}", paths);
        assert!(paths.contains(&"clojure.set"), "imports: {:?}", paths);
        // :refer'd var `join` must NOT be captured as a namespace.
        assert!(!paths.contains(&"join"), "leaked :refer var: {:?}", paths);
        // `:as` keyword must not appear.
        assert!(!paths.iter().any(|p| p.starts_with(':')));
    }

    #[test]
    fn clojure_top_level_require_captured() {
        let src = "(require '[clojure.test :as t])\n(require 'clojure.pprint)\n";
        let imports = extract_imports(src, "clojurescript");
        let paths: Vec<&str> = imports.iter().map(|i| i.raw_path.as_str()).collect();
        assert!(paths.contains(&"clojure.test"), "imports: {:?}", paths);
        assert!(paths.contains(&"clojure.pprint"), "imports: {:?}", paths);
    }

    #[test]
    fn clojure_resolve_munges_hyphens_and_emits_extensions() {
        let imp = RawImport {
            raw_path: "my-app.core-utils".to_string(),
            kind: "require".to_string(),
        };
        let candidates = resolve_import_candidates(&imp, "src/my_app/main.clj", "clojure", None);
        assert!(
            candidates.contains(&"src/my_app/core_utils.clj".to_string()),
            "candidates: {:?}",
            candidates
        );
        assert!(candidates.contains(&"src/my_app/core_utils.cljc".to_string()));
        assert!(candidates.contains(&"src/my_app/core_utils.cljs".to_string()));
    }

    #[test]
    fn clojure_resolve_skips_java_classes() {
        // A Java-class import (uppercase final segment) is external — no project
        // file candidates.
        let imp = RawImport {
            raw_path: "java.util.Date".to_string(),
            kind: "require".to_string(),
        };
        let candidates = resolve_import_candidates(&imp, "src/app/core.clj", "clojure", None);
        assert!(candidates.is_empty(), "got: {:?}", candidates);
    }

    #[test]
    fn unknown_language_returns_empty() {
        let imports = extract_imports("import blah", "brainfuck");
        assert!(imports.is_empty());
    }

    #[test]
    fn empty_source_returns_empty() {
        for lang in ["rust", "python", "javascript", "java", "c", "go"] {
            assert!(
                extract_imports("", lang).is_empty(),
                "lang {} on empty source should yield no imports",
                lang
            );
        }
    }

    // ========================================================================
    // Property tests
    // ========================================================================

    proptest! {
        /// Any well-formed `use crate::<a>::<b>::…` produces a Rust `use`
        /// import capturing the full path. Idempotent across whitespace.
        #[test]
        fn prop_rust_use_crate_paths_captured(
            segments in prop::collection::vec("[a-z][a-z0-9_]{0,10}", 1..5usize),
        ) {
            let path = format!("crate::{}", segments.join("::"));
            let src = format!("    use {};\n", path);
            let imports = extract_imports(&src, "rust");
            prop_assert!(imports.iter().any(|i| i.raw_path == path && i.kind == "use"),
                "missing `use {}` from imports {:?}", path, imports);
        }

        /// Python `import X.Y.Z` is always captured with the full dotted path.
        #[test]
        fn prop_python_import_captures_dotted_path(
            segments in prop::collection::vec("[a-z][a-z0-9_]{0,10}", 1..5usize),
        ) {
            let path = segments.join(".");
            let src = format!("import {}\n", path);
            let imports = extract_imports(&src, "python");
            prop_assert!(imports.iter().any(|i| i.raw_path == path),
                "missing `import {}` from imports {:?}", path, imports);
        }

        /// Extraction is idempotent: running extract_imports twice on the
        /// same source yields the same result (regex-based, no state).
        #[test]
        fn prop_extract_idempotent(
            segments in prop::collection::vec("[a-z][a-z0-9_]{0,8}", 1..4usize),
            lang_idx in 0usize..3,
        ) {
            let (src, lang) = match lang_idx {
                0 => (format!("use crate::{};\n", segments.join("::")), "rust"),
                1 => (format!("import {}\n", segments.join(".")), "python"),
                _ => (format!("import {{ x }} from '{}';\n", segments.join("/")), "javascript"),
            };
            let a = extract_imports(&src, lang);
            let b = extract_imports(&src, lang);
            prop_assert_eq!(a.len(), b.len());
            for (x, y) in a.iter().zip(b.iter()) {
                prop_assert_eq!(&x.raw_path, &y.raw_path);
                prop_assert_eq!(&x.kind, &y.kind);
            }
        }

        /// normalize_path handles `.` and `..` like POSIX — `..` pops last,
        /// `.` and empty segments are dropped.
        #[test]
        fn prop_normalize_path_resolves_dot_dot(
            parts in prop::collection::vec("[a-z0-9]{1,8}", 1..6usize),
        ) {
            let absolute = parts.join("/");
            prop_assert_eq!(normalize_path(&absolute), absolute.clone());
            // Adding a /./ mid-path must not change the output.
            let with_dot = format!("{}/./{}", parts[0], parts[1..].join("/"));
            prop_assert_eq!(normalize_path(&with_dot), absolute);
        }

        /// Rust: all three forms (use crate::, mod, extern crate) are
        /// captured as distinct import kinds.
        #[test]
        fn prop_rust_imports_capture_use_mod_extern(
            use_seg in "[a-z]{1,6}",
            mod_name in "[a-z]{1,6}",
            crate_name in "[a-z]{1,6}",
        ) {
            let src = format!(
                "use crate::{}::inner;\npub mod {};\nextern crate {};\n",
                use_seg, mod_name, crate_name,
            );
            let imports = extract_imports(&src, "rust");
            prop_assert!(imports.iter().any(|i| i.kind == "use"));
            prop_assert!(imports.iter().any(|i| i.kind == "mod"));
            prop_assert!(imports.iter().any(|i| i.kind == "extern_crate"));
        }

        /// JavaScript: `import x from 'y'`, `require('y')`, and
        /// `export from 'y'` each produce a distinct entry.
        #[test]
        fn prop_javascript_imports_capture_es_modules_and_require(
            a in "[a-z]{1,6}",
            b in "[a-z]{1,6}",
            c in "[a-z]{1,6}",
        ) {
            let src = format!(
                "import x from '{}';\nconst y = require('{}');\nexport {{ z }} from '{}';\n",
                a, b, c,
            );
            let imports = extract_imports(&src, "javascript");
            let paths: Vec<&str> = imports.iter().map(|i| i.raw_path.as_str()).collect();
            prop_assert!(paths.contains(&a.as_str()));
            prop_assert!(paths.contains(&b.as_str()));
            prop_assert!(paths.contains(&c.as_str()));
        }

        /// Java: `import pkg.Class;` and `import static pkg.X;` both captured.
        #[test]
        fn prop_java_imports_capture_regular_and_static(
            pkg in "[a-z]{1,6}",
            cls in "[A-Z][a-z]{1,6}",
        ) {
            let src = format!(
                "package x;\nimport {pkg}.{cls};\nimport static {pkg}.X.CONST;\n",
                pkg = pkg, cls = cls,
            );
            let imports = extract_imports(&src, "java");
            prop_assert!(imports.iter().any(|i| i.raw_path.contains(&cls.to_string())));
            prop_assert!(imports.iter().any(|i| i.raw_path.contains("CONST")));
        }

        /// Go: any `"pkg/path"` inside source becomes an import (the regex
        /// matches any double-quoted path-like string).
        #[test]
        fn prop_go_imports_capture_quoted_paths(
            a in "[a-z]{1,6}",
            b in "[a-z]{1,6}",
        ) {
            let src = format!("import (\n\t\"{}/{}\"\n)\n", a, b);
            let imports = extract_imports(&src, "go");
            let expected = format!("{}/{}", a, b);
            prop_assert!(imports.iter().any(|i| i.raw_path == expected));
        }

        /// C/C++: both `#include <header.h>` and `#include "header.h"` are
        /// captured with the `include` kind.
        #[test]
        fn prop_c_imports_capture_angle_and_quote_forms(
            sys in "[a-z]{1,8}",
            local in "[a-z]{1,8}",
        ) {
            let src = format!("#include <{}.h>\n#include \"{}.h\"\n", sys, local);
            let imports = extract_imports(&src, "c");
            let paths: Vec<&str> = imports.iter().map(|i| i.raw_path.as_str()).collect();
            let sys_hdr = format!("{}.h", sys);
            let local_hdr = format!("{}.h", local);
            prop_assert!(paths.contains(&sys_hdr.as_str()));
            prop_assert!(paths.contains(&local_hdr.as_str()));
        }

        /// TypeScript uses the same regex set as JavaScript — identical
        /// outputs for the same input.
        #[test]
        fn prop_typescript_output_matches_javascript(
            name in "[a-z]{2,8}",
        ) {
            let src = format!("import {{ Foo }} from './{}';\n", name);
            let js = extract_imports(&src, "javascript");
            let ts = extract_imports(&src, "typescript");
            prop_assert_eq!(js.len(), ts.len());
            for (a, b) in js.iter().zip(ts.iter()) {
                prop_assert_eq!(&a.raw_path, &b.raw_path);
            }
        }
    }
}
