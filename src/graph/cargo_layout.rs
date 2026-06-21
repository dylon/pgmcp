//! Cargo workspace crate layout: maps a crate's *library identifier* (the name
//! used in `use <ident>::…`) to that crate's source directory, project-root
//! relative.
//!
//! ## Why this exists
//!
//! pgmcp resolves Rust `use` imports to target files by path convention
//! (`src/graph/import_extractor.rs`). For `use crate::…` / `use super::…` the
//! target directory is derivable from the *importing* file's own path. But a
//! cross-crate `use mettail_prattail::wpda::Foo;` names the crate's **library
//! identifier** (`mettail_prattail`), which need not equal the crate's
//! **directory** (`prattail/`). The mapping ident → directory lives only in the
//! workspace `Cargo.toml` files (`[package] name`, optional `[lib] name`,
//! optional `[lib] path`). Without it, every inter-crate edge is unresolvable
//! and the module-level coupling metrics degenerate to `Ca=Ce=0`.
//!
//! A `CrateLayout` is built once per project from its member manifests and
//! threaded through the resolver. It is the per-project building block that the
//! cross-project [`crate::graph::workspace_crate_map::WorkspaceCrateMap`]
//! composes.
//!
//! ## Identifier rule
//!
//! Cargo derives a crate's `use`-identifier as `[lib] name` when present, else
//! `[package] name` with `-` normalized to `_` (cargo forbids `-` in a lib
//! name, so no normalization is needed for an explicit `[lib] name`). The
//! crate's source directory is the parent of `[lib] path` when set, else
//! `<member-dir>/src`. Bare `[[bin]]`-only packages expose no importable lib
//! and are skipped; virtual workspace manifests (no `[package]`) are skipped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Map from a crate's library identifier (as it appears as the first segment of
/// a `use` path, e.g. `mettail_prattail`) to that crate's source directory,
/// **project-root-relative** with `/` separators (e.g. `prattail/src`). The
/// relative convention matches `indexed_files.relative_path`, the key space of
/// the resolver's `file_paths` lookup.
#[derive(Debug, Clone, Default)]
pub struct CrateLayout {
    ident_to_src_dir: HashMap<String, String>,
}

impl CrateLayout {
    /// Build the layout by walking `project_root` for member `Cargo.toml`s.
    /// `project_root` is an absolute filesystem path (`projects.path`). Never
    /// fails: an unreadable/un-parseable manifest is skipped, yielding a
    /// possibly-empty layout (a non-Rust or manifest-less project is simply
    /// empty, and the resolver falls back to its existing behavior).
    pub fn build_for_project(project_root: &str) -> Self {
        let root = Path::new(project_root);
        let mut ident_to_src_dir = HashMap::new();
        for manifest in find_cargo_tomls(root) {
            let Ok(text) = std::fs::read_to_string(&manifest) else {
                continue;
            };
            let Ok(val) = text.parse::<toml::Value>() else {
                continue;
            };
            // Virtual workspace manifest (no `[package]`) defines no crate ident.
            let Some(pkg) = val.get("package").and_then(|v| v.as_table()) else {
                continue;
            };
            let Some(pkg_name) = pkg.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let lib = val.get("lib").and_then(|v| v.as_table());
            // ident = [lib].name if present else package name with '-'→'_'.
            let ident = lib
                .and_then(|l| l.get("name"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| pkg_name.replace('-', "_"));

            let member_dir_rel = manifest
                .parent()
                .and_then(|p| p.strip_prefix(root).ok())
                .map(rel_to_slash)
                .unwrap_or_default();

            // src_dir = parent of [lib].path if set, else <member-dir>/src.
            let src_dir = match lib.and_then(|l| l.get("path")).and_then(|v| v.as_str()) {
                Some(lib_path) => {
                    let parent = Path::new(lib_path)
                        .parent()
                        .map(rel_to_slash)
                        .unwrap_or_default();
                    join_rel(&member_dir_rel, &parent)
                }
                None => join_rel(&member_dir_rel, "src"),
            };
            ident_to_src_dir.insert(ident, src_dir);
        }
        Self { ident_to_src_dir }
    }

    /// Construct directly from an `ident → src_dir` map. Test-only: production
    /// code builds layouts from manifests via [`Self::build_for_project`]; the
    /// in-crate unit tests (here, `metrics`, `import_extractor`) use this to seed
    /// a known mapping without touching the filesystem.
    #[cfg(test)]
    pub fn from_map(ident_to_src_dir: HashMap<String, String>) -> Self {
        Self { ident_to_src_dir }
    }

    /// True when no crate identifiers were discovered (non-Rust / manifest-less
    /// project). Callers use this to fall back to non-crate-aware behavior.
    pub fn is_empty(&self) -> bool {
        self.ident_to_src_dir.is_empty()
    }

    /// The source directory for a crate identifier, if known.
    pub fn src_dir_for(&self, ident: &str) -> Option<&str> {
        self.ident_to_src_dir.get(ident).map(String::as_str)
    }

    /// Iterate `(ident, src_dir)` pairs — used to fold per-project layouts into
    /// the workspace-global [`crate::graph::workspace_crate_map::WorkspaceCrateMap`].
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.ident_to_src_dir
            .iter()
            .map(|(i, d)| (i.as_str(), d.as_str()))
    }

    /// Inverse lookup: the crate that owns a project-relative file path, as
    /// `(ident, src_dir)`. Longest matching `src_dir` prefix wins so a nested
    /// member (`crates/foo/src`) is preferred over a containing crate's
    /// directory. Returns `None` for files outside any crate source tree
    /// (e.g. `build.rs`, top-level docs), which the caller buckets by directory
    /// depth instead.
    pub fn crate_of_path(&self, relative_path: &str) -> Option<(&str, &str)> {
        let mut best: Option<(&str, &str)> = None;
        for (ident, src_dir) in &self.ident_to_src_dir {
            // A file belongs to the crate when it lives under `<src_dir>/`.
            if path_is_under(relative_path, src_dir)
                && best.is_none_or(|(_, b)| src_dir.len() > b.len())
            {
                best = Some((ident.as_str(), src_dir.as_str()));
            }
        }
        best
    }
}

/// `true` when `path` is `dir` itself or lives under `dir/`. Empty `dir`
/// (project root) matches everything.
fn path_is_under(path: &str, dir: &str) -> bool {
    if dir.is_empty() {
        return true;
    }
    path == dir
        || (path.len() > dir.len() && path.starts_with(dir) && path.as_bytes()[dir.len()] == b'/')
}

/// Join two project-relative path fragments with `/`, dropping empties.
fn join_rel(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a}/{b}"),
    }
}

/// Render a relative `Path` with `/` separators (the `relative_path` convention).
fn rel_to_slash(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Bounded recursive walk for `Cargo.toml` files under `root`, skipping
/// `target`/`.git`/`node_modules`/`.cargo` and capping depth + count so a huge
/// tree cannot stall the caller. Mirrors `src/deps/manifest.rs::find_cargo_tomls`
/// (duplicated deliberately to avoid coupling the graph layer to the deps
/// module; the walk is trivial and bound-identical).
fn find_cargo_tomls(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 6 || out.len() > 256 {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if matches!(name, "target" | ".git" | "node_modules" | ".cargo") {
                    continue;
                }
                stack.push((p, depth + 1));
            } else if p.file_name().and_then(|n| n.to_str()) == Some("Cargo.toml") {
                out.push(p);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(pairs: &[(&str, &str)]) -> CrateLayout {
        CrateLayout::from_map(
            pairs
                .iter()
                .map(|(i, d)| (i.to_string(), d.to_string()))
                .collect(),
        )
    }

    #[test]
    fn crate_of_path_longest_prefix_wins() {
        let l = layout(&[
            ("workspace_root", "src"),
            ("mettail_prattail", "prattail/src"),
            ("mettail_ast", "ast/src"),
        ]);
        assert_eq!(
            l.crate_of_path("prattail/src/wpda/walker.rs"),
            Some(("mettail_prattail", "prattail/src"))
        );
        assert_eq!(
            l.crate_of_path("ast/src/language.rs"),
            Some(("mettail_ast", "ast/src"))
        );
        // root crate file
        assert_eq!(
            l.crate_of_path("src/main.rs"),
            Some(("workspace_root", "src"))
        );
        // outside any crate src tree
        assert_eq!(l.crate_of_path("prattail/build.rs"), None);
        assert_eq!(l.crate_of_path("README.md"), None);
    }

    #[test]
    fn path_is_under_respects_segment_boundary() {
        assert!(path_is_under("prattail/src/x.rs", "prattail/src"));
        assert!(path_is_under("prattail/src", "prattail/src"));
        // must not match a sibling whose name is a string-prefix
        assert!(!path_is_under("prattail/src2/x.rs", "prattail/src"));
        assert!(!path_is_under("prattailX/src/x.rs", "prattail/src"));
    }

    #[test]
    fn build_for_project_handles_ident_ne_directory() {
        // Directory `prattail/` exposes lib ident `mettail_prattail`; a
        // hyphenated package name with no `[lib]` normalizes `-`→`_`; a virtual
        // workspace root contributes nothing.
        let root = std::env::temp_dir().join(format!("pgmcp_cargo_layout_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mk = |rel: &str, body: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().expect("has parent")).expect("mkdir");
            std::fs::write(&p, body).expect("write manifest");
        };
        // virtual workspace root (no [package])
        mk(
            "Cargo.toml",
            "[workspace]\nmembers = [\"prattail\", \"ast\", \"cli\"]\n",
        );
        // ident != directory via [lib] name
        mk(
            "prattail/Cargo.toml",
            "[package]\nname = \"mettail-prattail\"\n[lib]\nname = \"mettail_prattail\"\n",
        );
        // ident from hyphenated package name (no [lib])
        mk("ast/Cargo.toml", "[package]\nname = \"mettail-ast\"\n");
        // bin-only package (no importable lib) still exposes its package ident
        mk("cli/Cargo.toml", "[package]\nname = \"cli\"\n");
        // custom [lib] path
        mk(
            "weird/Cargo.toml",
            "[package]\nname = \"weird\"\n[lib]\nname = \"weird\"\npath = \"lib/entry.rs\"\n",
        );

        let l = CrateLayout::build_for_project(root.to_str().expect("utf8"));
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(l.src_dir_for("mettail_prattail"), Some("prattail/src"));
        assert_eq!(l.src_dir_for("mettail_ast"), Some("ast/src"));
        assert_eq!(l.src_dir_for("cli"), Some("cli/src"));
        assert_eq!(l.src_dir_for("weird"), Some("weird/lib"));
        // virtual root contributed no ident
        assert!(l.src_dir_for("workspace").is_none());
    }
}
