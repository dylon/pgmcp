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
