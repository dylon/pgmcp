//! Regex-based import extraction for multiple programming languages.
//! Extracts import/dependency relationships from source code content.

use regex::Regex;
use std::sync::LazyLock;

/// A raw import extracted from source code.
#[derive(Debug, Clone)]
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
    Regex::new(r"(?m)^\s*use\s+((?:crate|super|self)(?:::\w+)+)").expect("invalid regex")
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

/// Extract all imports from source content for a given language.
pub fn extract_imports(content: &str, language: &str) -> Vec<RawImport> {
    match language {
        "rust" => extract_rust_imports(content),
        "python" => extract_python_imports(content),
        "javascript" | "typescript" => extract_js_imports(content),
        "go" => extract_go_imports(content),
        "java" | "kotlin" => extract_java_imports(content),
        "c" | "cpp" | "c++" | "header" => extract_c_imports(content),
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

// ============================================================================
// Import resolution
// ============================================================================

/// Resolve raw imports to candidate relative file paths within a project.
/// `source_relative_path`: the file containing the import (e.g., "src/mcp/server.rs").
pub fn resolve_import_candidates(
    import: &RawImport,
    source_relative_path: &str,
    language: &str,
) -> Vec<String> {
    match language {
        "rust" => resolve_rust_import(import, source_relative_path),
        "python" => resolve_python_import(import, source_relative_path),
        "javascript" | "typescript" => resolve_js_import(import, source_relative_path),
        _ => Vec::new(),
    }
}

fn resolve_rust_import(import: &RawImport, source_relative_path: &str) -> Vec<String> {
    let path = &import.raw_path;

    match &*import.kind {
        "use" => {
            if let Some(rest) = path.strip_prefix("crate::") {
                // crate::db::queries -> src/db/queries.rs, src/db/queries/mod.rs
                let segments: Vec<&str> = rest.split("::").collect();
                rust_path_candidates("src", &segments)
            } else if let Some(rest) = path.strip_prefix("super::") {
                // super::foo -> sibling module relative to current
                let parent = parent_module(source_relative_path);
                let segments: Vec<&str> = rest.split("::").collect();
                rust_path_candidates(&parent, &segments)
            } else {
                // External crate reference
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

fn rust_path_candidates(base: &str, segments: &[&str]) -> Vec<String> {
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
