//! Cargo-manifest dependency extraction. For a project we walk its tree for
//! `Cargo.toml` files and resolve each `[dependencies]` / `[dev-dependencies]` /
//! `[build-dependencies]` entry to another indexed project — by local `path`
//! (precise), or by name for `git`/registry deps (a project named like the
//! crate). Resolved edges are upserted as `source='cargo'`; vanished ones are
//! closed (bitemporal history kept). In Rust the manifest *is* the import graph,
//! so this comprehensively covers cross-project coupling for the workspace.

use std::path::{Path, PathBuf};

use sqlx::PgPool;

use crate::deps::{DepSource, store};

/// Index all Cargo manifests under `project_root`, upserting cross-project
/// dependency edges for `project_id` and closing vanished ones. Returns
/// `(upserted, closed)`.
pub async fn index_project_manifests(
    pool: &PgPool,
    project_id: i32,
    project_root: &str,
) -> (usize, u64) {
    let run_start = chrono::Utc::now();
    let mut upserted = 0usize;
    for manifest in find_cargo_tomls(Path::new(project_root)) {
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let Ok(val) = text.parse::<toml::Value>() else {
            continue;
        };
        let manifest_dir = manifest
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(project_root));
        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            let Some(tbl) = val.get(section).and_then(|v| v.as_table()) else {
                continue;
            };
            for (dep_name, spec) in tbl {
                if let Some((kind, dep_id, conf)) =
                    resolve_dep(pool, &manifest_dir, dep_name, spec).await
                    && dep_id != project_id
                    && store::upsert_dependency(
                        pool,
                        project_id,
                        dep_id,
                        Some(dep_name),
                        Some(kind),
                        DepSource::Cargo,
                        conf,
                    )
                    .await
                    .is_ok()
                {
                    upserted += 1;
                }
            }
        }
    }
    let closed = store::close_stale(pool, project_id, DepSource::Cargo, run_start)
        .await
        .unwrap_or(0);
    (upserted, closed)
}

/// Resolve a dependency spec to `(kind, dependency_project_id, confidence)`.
async fn resolve_dep(
    pool: &PgPool,
    manifest_dir: &Path,
    dep_name: &str,
    spec: &toml::Value,
) -> Option<(&'static str, i32, f64)> {
    // Local `path = "../X"` — the precise project link.
    if let Some(path) = spec.get("path").and_then(|v| v.as_str())
        && let Ok(canon) = manifest_dir.join(path).canonicalize()
        && let Some(id) = project_by_path(pool, &canon).await
    {
        return Some(("path", id, 1.0));
    }
    // `git = "…"` — no local path; match the crate name to a project.
    if spec.get("git").is_some() {
        return project_by_name(pool, dep_name)
            .await
            .map(|id| ("git", id, 0.8));
    }
    // Registry / version dep — match the crate name to a project (one of ours).
    project_by_name(pool, dep_name)
        .await
        .map(|id| ("registry", id, 0.7))
}

pub(crate) async fn project_by_path(pool: &PgPool, path: &Path) -> Option<i32> {
    let p = path.to_string_lossy();
    let p_slash = format!("{}/", p.trim_end_matches('/'));
    sqlx::query_scalar("SELECT id FROM projects WHERE path = $1 OR path = $2 LIMIT 1")
        .bind(p.as_ref())
        .bind(&p_slash)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

pub(crate) async fn project_by_name(pool: &PgPool, name: &str) -> Option<i32> {
    sqlx::query_scalar("SELECT id FROM projects WHERE name = $1 LIMIT 1")
        .bind(name)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

/// Bounded recursive walk for `Cargo.toml` files under `root`, skipping
/// `target`/`.git`/`node_modules`/`.cargo` and capping depth + count so a huge
/// tree can't stall the cron.
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

    #[test]
    fn find_cargo_tomls_collects_real_manifests_and_skips_build_dirs() {
        // Build-artifact / VCS / vendored dirs must be skipped so a vendored or
        // compiled `Cargo.toml` is never mistaken for a first-party dependency.
        let root = std::env::temp_dir().join(format!("pgmcp_manifest_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mk = |rel: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().expect("has parent")).expect("mkdir");
            std::fs::write(&p, "[package]\nname = \"x\"\n").expect("write manifest");
        };
        mk("Cargo.toml");
        mk("crate-a/Cargo.toml");
        mk("target/debug/Cargo.toml"); // skipped: build output
        mk(".git/Cargo.toml"); // skipped: VCS
        mk("node_modules/p/Cargo.toml"); // skipped: vendored
        mk(".cargo/Cargo.toml"); // skipped: cargo home

        let found = find_cargo_tomls(&root);
        let rels: std::collections::HashSet<String> = found
            .iter()
            .map(|p| {
                p.strip_prefix(&root)
                    .expect("under root")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let _ = std::fs::remove_dir_all(&root);

        assert!(rels.contains("Cargo.toml"), "root manifest found: {rels:?}");
        assert!(
            rels.contains("crate-a/Cargo.toml"),
            "nested manifest found: {rels:?}"
        );
        for skipped in ["target", ".git", "node_modules", ".cargo"] {
            assert!(
                !rels.iter().any(|r| r.contains(skipped)),
                "{skipped}/ must be skipped: {rels:?}"
            );
        }
        assert_eq!(
            found.len(),
            2,
            "exactly the two first-party manifests: {rels:?}"
        );
    }
}
