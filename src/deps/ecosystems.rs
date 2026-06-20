//! Multi-ecosystem manifest indexing (ADR-027 Stage 2): npm / pypi / go / maven
//! / lake, complementing the Cargo path in `manifest.rs`. Each ecosystem parser
//! finds its manifests, extracts dependency names (+ local paths where the
//! format exposes them), resolves them to indexed projects (local path → exact
//! 1.0; else name match → 0.7), and upserts with the ecosystem's `DepSource`.
//! `index_all_manifests` is the dispatcher the cron calls.

use std::path::{Path, PathBuf};

use serde_json::Value as Json;
use sqlx::PgPool;

use crate::deps::DepSource;
use crate::deps::manifest::{index_project_manifests, project_by_name, project_by_path};
use crate::deps::store;

/// One parsed dependency: its name and (if the manifest exposes one) a local
/// filesystem path relative to the manifest dir.
struct DepEntry {
    name: String,
    local_path: Option<String>,
}

/// All declared dependency package NAMES in a manifest file, by filename — for
/// CVE matching (ADR-027 E6). Reuses the per-ecosystem parsers; Cargo.toml is
/// handled here via a `[dependencies]` table scan (the project-graph path in
/// `manifest.rs` resolves Cargo deps to projects, but CVE matching wants the raw
/// names). Returns empty for unrecognized files.
pub(crate) fn package_names(file_name: &str, content: &str) -> Vec<String> {
    let dir = Path::new("");
    let entries = match file_name {
        "package.json" => parse_npm(dir, content),
        "pyproject.toml" | "requirements.txt" => parse_pypi(dir, content),
        "go.mod" => parse_go(dir, content),
        "pom.xml" => parse_maven(dir, content),
        "lake-manifest.json" | "lakefile.lean" => parse_lake(dir, content),
        "Cargo.toml" => return cargo_dep_names(content),
        _ => Vec::new(),
    };
    entries.into_iter().map(|e| e.name).collect()
}

/// `[dependencies]` / `[dev-dependencies]` / `[build-dependencies]` keys of a Cargo.toml.
fn cargo_dep_names(content: &str) -> Vec<String> {
    let Ok(val) = content.parse::<toml::Value>() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for sec in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(tbl) = val.get(sec).and_then(|v| v.as_table()) {
            out.extend(tbl.keys().cloned());
        }
    }
    out
}

/// Index every ecosystem's manifests for a project. Returns (upserted, closed)
/// summed across ecosystems (Cargo via `index_project_manifests`, then the rest).
pub async fn index_all_manifests(
    pool: &PgPool,
    project_id: i32,
    project_root: &str,
) -> (usize, u64) {
    let (mut up, mut closed) = index_project_manifests(pool, project_id, project_root).await;
    let root = Path::new(project_root);
    for (u, c) in [
        index_ecosystem(pool, project_id, root, DepSource::Npm, parse_npm).await,
        index_ecosystem(pool, project_id, root, DepSource::Pypi, parse_pypi).await,
        index_ecosystem(pool, project_id, root, DepSource::Go, parse_go).await,
        index_ecosystem(pool, project_id, root, DepSource::Maven, parse_maven).await,
        index_ecosystem(pool, project_id, root, DepSource::Lake, parse_lake).await,
    ] {
        up += u;
        closed += c;
    }
    (up, closed)
}

/// Generic per-ecosystem indexer: walk for manifests `source` recognizes, parse
/// each, resolve + upsert, then close stale edges of that source.
async fn index_ecosystem(
    pool: &PgPool,
    project_id: i32,
    project_root: &Path,
    source: DepSource,
    parse: fn(&Path, &str) -> Vec<DepEntry>,
) -> (usize, u64) {
    let run_start = chrono::Utc::now();
    let mut upserted = 0usize;
    for manifest in find_manifests(project_root, source) {
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let manifest_dir = manifest
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| project_root.to_path_buf());
        for entry in parse(&manifest_dir, &text) {
            // Local path → exact link (1.0); else match the name to a project.
            let resolved: Option<(&'static str, i32, f64)> = match &entry.local_path {
                Some(lp) => match manifest_dir.join(lp).canonicalize() {
                    Ok(canon) => project_by_path(pool, &canon)
                        .await
                        .map(|id| ("path", id, 1.0)),
                    Err(_) => None,
                },
                None => None,
            };
            let resolved = match resolved {
                Some(r) => Some(r),
                None => project_by_name(pool, &entry.name)
                    .await
                    .map(|id| ("registry", id, 0.7)),
            };
            if let Some((kind, dep_id, conf)) = resolved
                && dep_id != project_id
                && store::upsert_dependency(
                    pool,
                    project_id,
                    dep_id,
                    Some(&entry.name),
                    Some(kind),
                    source,
                    conf,
                )
                .await
                .is_ok()
            {
                upserted += 1;
            }
        }
    }
    let closed = store::close_stale(pool, project_id, source, run_start)
        .await
        .unwrap_or(0);
    (upserted, closed)
}

/// Whether `file_name` is a manifest the ecosystem recognizes.
fn is_manifest(source: DepSource, file_name: &str) -> bool {
    match source {
        DepSource::Npm => file_name == "package.json",
        DepSource::Pypi => file_name == "pyproject.toml" || file_name == "requirements.txt",
        DepSource::Go => file_name == "go.mod",
        DepSource::Maven => file_name == "pom.xml",
        DepSource::Lake => file_name == "lake-manifest.json" || file_name == "lakefile.lean",
        _ => false,
    }
}

/// Bounded recursive walk for an ecosystem's manifests, skipping build output,
/// VCS, and vendored trees (mirrors `manifest::find_cargo_tomls`).
fn find_manifests(root: &Path, source: DepSource) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    const MAX_DEPTH: usize = 8;
    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if p.is_dir() {
                if matches!(
                    name,
                    "target" | ".git" | "node_modules" | ".cargo" | "vendor" | ".lake" | "dist"
                ) {
                    continue;
                }
                stack.push((p, depth + 1));
            } else if is_manifest(source, name) {
                out.push(p);
            }
        }
    }
    out
}

/// Strip a Python/PEP-508 / version-spec'd requirement down to its package name.
fn pkg_name(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('#') || raw.starts_with('-') {
        return None;
    }
    let name: String = raw
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
        .collect();
    (!name.is_empty()).then_some(name)
}

fn parse_npm(_dir: &Path, text: &str) -> Vec<DepEntry> {
    let Ok(json) = serde_json::from_str::<Json>(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for section in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(obj) = json.get(section).and_then(|v| v.as_object()) {
            for (name, spec) in obj {
                // `"file:../x"` is a local workspace link.
                let local_path = spec
                    .as_str()
                    .and_then(|s| s.strip_prefix("file:"))
                    .map(str::to_string);
                out.push(DepEntry {
                    name: name.clone(),
                    local_path,
                });
            }
        }
    }
    out
}

fn parse_pypi(_dir: &Path, text: &str) -> Vec<DepEntry> {
    // requirements.txt: one requirement per line.
    if let Ok(val) = text.parse::<toml::Value>() {
        let mut out = Vec::new();
        // PEP 621: [project] dependencies = ["name>=x", ...]
        if let Some(arr) = val
            .get("project")
            .and_then(|p| p.get("dependencies"))
            .and_then(|d| d.as_array())
        {
            for item in arr {
                if let Some(s) = item.as_str()
                    && let Some(n) = pkg_name(s)
                {
                    out.push(DepEntry {
                        name: n,
                        local_path: None,
                    });
                }
            }
        }
        // Poetry: [tool.poetry.dependencies] table (skip the `python` pin).
        if let Some(tbl) = val
            .get("tool")
            .and_then(|t| t.get("poetry"))
            .and_then(|p| p.get("dependencies"))
            .and_then(|d| d.as_table())
        {
            for name in tbl.keys() {
                if name != "python" {
                    out.push(DepEntry {
                        name: name.clone(),
                        local_path: None,
                    });
                }
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    // Fallback: treat as requirements.txt.
    text.lines()
        .filter_map(pkg_name)
        .map(|name| DepEntry {
            name,
            local_path: None,
        })
        .collect()
}

fn parse_go(_dir: &Path, text: &str) -> Vec<DepEntry> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with("require (") {
            in_block = true;
            continue;
        }
        if in_block && l == ")" {
            in_block = false;
            continue;
        }
        let spec = if in_block {
            Some(l)
        } else {
            l.strip_prefix("require ").map(str::trim)
        };
        if let Some(spec) = spec {
            // `module/path v1.2.3` → match on the last path segment (the project
            // basename convention indexed projects use).
            if let Some(modpath) = spec.split_whitespace().next()
                && !modpath.is_empty()
                && let Some(seg) = modpath.rsplit('/').next()
            {
                out.push(DepEntry {
                    name: seg.to_string(),
                    local_path: None,
                });
            }
        }
    }
    out
}

fn parse_maven(_dir: &Path, text: &str) -> Vec<DepEntry> {
    // No XML dep: extract <artifactId>…</artifactId> occurrences. A pom lists
    // an artifactId for the project itself and each dependency; matching only
    // against indexed projects (by name) filters the noise.
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<artifactId>") {
        let after = &rest[start + "<artifactId>".len()..];
        if let Some(end) = after.find("</artifactId>") {
            let name = after[..end].trim().to_string();
            if !name.is_empty() {
                out.push(DepEntry {
                    name,
                    local_path: None,
                });
            }
            rest = &after[end..];
        } else {
            break;
        }
    }
    out
}

fn parse_lake(_dir: &Path, text: &str) -> Vec<DepEntry> {
    // lake-manifest.json: {"packages":[{"name":"…"}, …]}.
    if let Ok(json) = serde_json::from_str::<Json>(text)
        && let Some(pkgs) = json.get("packages").and_then(|p| p.as_array())
    {
        return pkgs
            .iter()
            .filter_map(|p| p.get("name").and_then(|n| n.as_str()))
            .map(|name| DepEntry {
                name: name.to_string(),
                local_path: None,
            })
            .collect();
    }
    // lakefile.lean: `require X from …` lines.
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("require "))
        .filter_map(|r| r.split_whitespace().next())
        .map(|name| DepEntry {
            name: name.to_string(),
            local_path: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn npm_extracts_deps_and_local() {
        let json = r#"{"dependencies":{"react":"^18.0.0","mylib":"file:../mylib"},
                       "devDependencies":{"jest":"^29.0.0"}}"#;
        let deps = parse_npm(Path::new("/"), json);
        let names: Vec<&str> = deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"react"));
        assert!(names.contains(&"jest"));
        let mylib = deps.iter().find(|d| d.name == "mylib").unwrap();
        assert_eq!(mylib.local_path.as_deref(), Some("../mylib"));
    }

    #[test]
    fn pypi_pep621_and_requirements() {
        let pyproject = "[project]\ndependencies = [\"requests>=2.0\", \"numpy\"]\n";
        let n: Vec<String> = parse_pypi(Path::new("/"), pyproject)
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert!(n.contains(&"requests".to_string()), "{n:?}");
        assert!(n.contains(&"numpy".to_string()));
        let reqs = "# comment\nflask==2.0\n-r other.txt\nrich\n";
        let n2: Vec<String> = parse_pypi(Path::new("/"), reqs)
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(n2, vec!["flask", "rich"], "{n2:?}");
    }

    #[test]
    fn go_mod_block_and_inline() {
        let gomod = "module x\n\nrequire (\n\tgithub.com/foo/bar v1.2.3\n)\nrequire golang.org/x/sync v0.1.0\n";
        let n: Vec<String> = parse_go(Path::new("/"), gomod)
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert!(n.contains(&"bar".to_string()), "{n:?}");
        assert!(n.contains(&"sync".to_string()), "{n:?}");
    }

    #[test]
    fn maven_artifact_ids() {
        let pom = "<project><artifactId>self</artifactId><dependencies>\
                   <dependency><groupId>g</groupId><artifactId>guava</artifactId></dependency>\
                   </dependencies></project>";
        let n: Vec<String> = parse_maven(Path::new("/"), pom)
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert!(n.contains(&"guava".to_string()), "{n:?}");
    }

    #[test]
    fn lake_manifest_and_lakefile() {
        let manifest = r#"{"packages":[{"name":"mathlib"},{"name":"std4"}]}"#;
        let n: Vec<String> = parse_lake(Path::new("/"), manifest)
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(n, vec!["mathlib", "std4"], "{n:?}");
        let lakefile = "import Lake\nrequire mathlib from git \"…\"\n";
        let n2: Vec<String> = parse_lake(Path::new("/"), lakefile)
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(n2, vec!["mathlib"], "{n2:?}");
    }
}
