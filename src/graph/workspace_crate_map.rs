//! Workspace-global crate map: resolves a cross-crate `use <ident>::…` to a
//! target file **in another indexed project**, producing a cross-project import
//! edge.
//!
//! ## Why this exists
//!
//! [`crate::graph::cargo_layout::CrateLayout`] resolves cross-crate imports
//! within a single pgmcp project (a cargo workspace indexed as one project). But
//! a crate may live in a *different* indexed project (e.g. `f1r3node-rust` depended
//! on by a sibling repo). The per-project resolver cannot reach it because its
//! `file_paths` map holds only its own files. This map is built once per
//! graph-analysis pass over *all* projects and provides the Tier-2 fallback the
//! cron uses after the per-project (Tier-1) lookup misses.
//!
//! ## Worktree safety (critical)
//!
//! A workspace is frequently indexed as several projects that are git-worktree
//! clones of each other (`mettail-rust`, `mettail-rust-bindercoll`, …) — same
//! crate identifiers, same relative paths, different `project_id`. A cross-crate
//! `use` must **never** resolve into a sibling clone (that would fabricate edges
//! between a project and its own worktree). [`WorkspaceCrateMap::pick_entry`]
//! enforces this: it prefers the source project itself, then a precise cargo
//! `path=` dependency edge, then the worktree-group **main** only, and fails
//! closed rather than fan out into a clone. Worktree families are derived by the
//! existing pure [`crate::hierarchy::grouping::derive_groups`].

use std::collections::{HashMap, HashSet};

use sqlx::PgPool;

use crate::graph::cargo_layout::CrateLayout;
use crate::graph::import_extractor::{RawImport, rust_path_candidates};
use crate::hierarchy::GroupRole;
use crate::hierarchy::grouping::derive_groups;

/// One crate, anywhere in the indexed workspace.
pub struct CrateEntry {
    pub project_id: i32,
    /// Crate source dir, relative to that project's root (`prattail/src`).
    pub src_dir: String,
    /// Worktree-family key; identical across clones of one workspace.
    pub group_key: String,
    /// Whether this project is its worktree family's main (shortest basename).
    pub is_group_main: bool,
}

/// All crates across all indexed projects, indexed by library identifier, plus
/// the auxiliary structures needed for worktree-safe cross-project resolution.
#[derive(Default)]
pub struct WorkspaceCrateMap {
    /// Per-project crate layout (Tier-1 intra-project resolution + bucketing).
    layouts: HashMap<i32, CrateLayout>,
    /// ident → every crate exposing it (usually 1; >1 only across clones/collisions).
    by_ident: HashMap<String, Vec<CrateEntry>>,
    /// project_id → (relative_path → file_id), for turning a resolved candidate
    /// path into a target file_id without a per-edge SELECT.
    file_paths: HashMap<i32, HashMap<String, i64>>,
    /// dependent_project_id → set of dependency_project_ids (precise cargo `path=`
    /// links from `project_dependencies`). The authoritative "which project is
    /// this dep" signal.
    dep_targets: HashMap<i32, HashSet<i32>>,
    /// project_id → (worktree group_key, is_group_main).
    group_of: HashMap<i32, (String, bool)>,
}

impl WorkspaceCrateMap {
    /// Build the map once per graph-analysis pass. `projects` is `(id, root_path)`
    /// for every indexed project (the same list `run_graph_analysis` iterates).
    pub async fn build(pool: &PgPool, projects: &[(i32, String)]) -> Result<Self, sqlx::Error> {
        // 1. Worktree families (reuse the pure, unit-tested derivation).
        let group_rows: Vec<(i32, String, Option<String>, Option<String>)> =
            sqlx::query_as("SELECT id, path, git_common_dir, git_root_commits FROM projects")
                .fetch_all(pool)
                .await?;
        let mut group_of: HashMap<i32, (String, bool)> = HashMap::new();
        for g in derive_groups(&group_rows) {
            for (pid, role) in g.members {
                group_of.insert(pid, (g.group_key.clone(), role == GroupRole::Main));
            }
        }

        // 2. Per-project crate layouts → by_ident.
        let mut layouts: HashMap<i32, CrateLayout> = HashMap::with_capacity(projects.len());
        let mut by_ident: HashMap<String, Vec<CrateEntry>> = HashMap::new();
        for (pid, path) in projects {
            let layout = CrateLayout::build_for_project(path);
            let (group_key, is_group_main) = group_of
                .get(pid)
                .cloned()
                .unwrap_or_else(|| (format!("singleton:{pid}"), true));
            for (ident, src_dir) in layout.entries() {
                by_ident
                    .entry(ident.to_string())
                    .or_default()
                    .push(CrateEntry {
                        project_id: *pid,
                        src_dir: src_dir.to_string(),
                        group_key: group_key.clone(),
                        is_group_main,
                    });
            }
            layouts.insert(*pid, layout);
        }

        // 3. All-projects file_paths (one scan; metadata only, no content).
        let fp_rows: Vec<(i32, String, i64)> =
            sqlx::query_as("SELECT project_id, relative_path, id FROM indexed_files")
                .fetch_all(pool)
                .await?;
        let mut file_paths: HashMap<i32, HashMap<String, i64>> = HashMap::new();
        for (pid, rel, id) in fp_rows {
            file_paths.entry(pid).or_default().insert(rel, id);
        }

        // 4. Precise cargo dependency edges.
        let dep_rows: Vec<(i32, i32)> = sqlx::query_as(
            "SELECT dependent_project_id, dependency_project_id FROM project_dependencies \
             WHERE valid_to IS NULL AND source = 'cargo'",
        )
        .fetch_all(pool)
        .await?;
        let mut dep_targets: HashMap<i32, HashSet<i32>> = HashMap::new();
        for (dependent, dependency) in dep_rows {
            dep_targets.entry(dependent).or_default().insert(dependency);
        }

        Ok(Self {
            layouts,
            by_ident,
            file_paths,
            dep_targets,
            group_of,
        })
    }

    /// This project's crate layout (Tier-1 resolution + crate-aware bucketing).
    pub fn layout_of(&self, project_id: i32) -> Option<&CrateLayout> {
        self.layouts.get(&project_id)
    }

    /// This project's worktree group key (`""` if unknown).
    pub fn group_key_of(&self, project_id: i32) -> &str {
        self.group_of
            .get(&project_id)
            .map(|(k, _)| k.as_str())
            .unwrap_or("")
    }

    /// Resolve a cross-crate `use <ident>::path::Item;` to `(target_file_id,
    /// target_project_id)` in another project, or `None` if `ident` is local
    /// (`crate`/`super`/`self`), external (std/tokio/…), or unresolvable.
    pub fn resolve_external_use(
        &self,
        import: &RawImport,
        source_project_id: i32,
        source_group_key: &str,
    ) -> Option<(i64, i32)> {
        if import.kind != "use" {
            return None;
        }
        let (ident, rest) = import.raw_path.split_once("::")?;
        if matches!(ident, "crate" | "super" | "self") {
            return None;
        }
        let entries = self.by_ident.get(ident)?;
        let chosen = self.pick_entry(entries, source_project_id, source_group_key)?;
        let segments: Vec<&str> = rest.split("::").collect();
        let candidates = rust_path_candidates(&chosen.src_dir, &segments);
        let fp = self.file_paths.get(&chosen.project_id)?;
        let tfid = candidates
            .iter()
            .find_map(|c| fp.get(c.as_str()))
            .copied()?;
        Some((tfid, chosen.project_id))
    }

    /// Choose the crate a cross-crate `use` refers to, worktree-safely.
    /// Priority: (1) the source project itself; (2) a precise cargo dependency
    /// edge; (3) the source's worktree-group **main** (never a clone); (4) a
    /// single foreign-group crate, else a foreign group-main, else `None`.
    fn pick_entry<'a>(
        &self,
        entries: &'a [CrateEntry],
        src_pid: i32,
        src_group: &str,
    ) -> Option<&'a CrateEntry> {
        // 1. Same project (local crate; Tier-1 normally wins, but safe).
        if let Some(e) = entries.iter().find(|e| e.project_id == src_pid) {
            return Some(e);
        }
        // 2. Authoritative precise cargo `path=` dependency.
        if let Some(deps) = self.dep_targets.get(&src_pid)
            && let Some(e) = entries.iter().find(|e| deps.contains(&e.project_id))
        {
            return Some(e);
        }
        // 3. Same worktree group → the MAIN only, never a clone.
        if let Some(e) = entries
            .iter()
            .find(|e| e.group_key == src_group && e.is_group_main)
        {
            return Some(e);
        }
        // If the only candidates are the source's own clones, fail closed.
        if entries.iter().all(|e| e.group_key == src_group) {
            return None;
        }
        // 4. A single foreign-group crate is unambiguous; otherwise a foreign
        // group-main; otherwise fail closed (ambiguous collision).
        let foreign: Vec<&CrateEntry> = entries
            .iter()
            .filter(|e| e.group_key != src_group)
            .collect();
        match foreign.as_slice() {
            [only] => Some(only),
            _ => foreign.into_iter().find(|e| e.is_group_main),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pid: i32, src: &str, group: &str, main: bool) -> CrateEntry {
        CrateEntry {
            project_id: pid,
            src_dir: src.to_string(),
            group_key: group.to_string(),
            is_group_main: main,
        }
    }

    /// Build a map with only the structures `pick_entry`/`resolve_external_use`
    /// consult (no DB).
    fn map_with(
        by_ident: HashMap<String, Vec<CrateEntry>>,
        file_paths: HashMap<i32, HashMap<String, i64>>,
        dep_targets: HashMap<i32, HashSet<i32>>,
    ) -> WorkspaceCrateMap {
        WorkspaceCrateMap {
            layouts: HashMap::new(),
            by_ident,
            file_paths,
            dep_targets,
            group_of: HashMap::new(),
        }
    }

    #[test]
    fn pick_entry_prefers_same_project() {
        let entries = vec![entry(1, "src", "g", true), entry(2, "src", "g", false)];
        let m = map_with(HashMap::new(), HashMap::new(), HashMap::new());
        let chosen = m.pick_entry(&entries, 2, "g").expect("some");
        assert_eq!(chosen.project_id, 2);
    }

    #[test]
    fn pick_entry_uses_precise_cargo_dep_over_name() {
        let entries = vec![entry(10, "src", "ga", true), entry(20, "src", "gb", true)];
        let mut deps = HashMap::new();
        deps.insert(5, HashSet::from([20]));
        let m = map_with(HashMap::new(), HashMap::new(), deps);
        let chosen = m.pick_entry(&entries, 5, "gc").expect("some");
        assert_eq!(chosen.project_id, 20, "should follow the cargo dep edge");
    }

    #[test]
    fn pick_entry_same_group_resolves_to_main_never_clone() {
        // Source project 1 is the main of group "g"; project 2 is its clone.
        let entries = vec![entry(1, "src", "g", true), entry(2, "src", "g", false)];
        let m = map_with(HashMap::new(), HashMap::new(), HashMap::new());
        // A different source in the same group resolves to the main (1), not the clone.
        let chosen = m.pick_entry(&entries, 99, "g").expect("some");
        assert_eq!(chosen.project_id, 1);
    }

    #[test]
    fn pick_entry_fails_closed_when_only_clones_of_source_group() {
        // The only entries are non-main clones in the source's own group.
        let entries = vec![entry(2, "src", "g", false), entry(3, "src", "g", false)];
        let m = map_with(HashMap::new(), HashMap::new(), HashMap::new());
        assert!(
            m.pick_entry(&entries, 2, "g").is_none()
                || m.pick_entry(&entries, 2, "g").map(|e| e.project_id) == Some(2),
            "must not resolve into a sibling clone"
        );
        // From a source NOT among the entries: still must not pick a clone.
        assert!(m.pick_entry(&entries, 99, "g").is_none());
    }

    #[test]
    fn pick_entry_single_foreign_group_resolves() {
        let entries = vec![entry(7, "lib/src", "other", true)];
        let m = map_with(HashMap::new(), HashMap::new(), HashMap::new());
        let chosen = m.pick_entry(&entries, 1, "mine").expect("some");
        assert_eq!(chosen.project_id, 7);
    }

    #[test]
    fn resolve_external_use_ignores_local_and_non_use() {
        let m = map_with(HashMap::new(), HashMap::new(), HashMap::new());
        for raw in ["crate::a::b", "super::a", "self::a"] {
            let imp = RawImport {
                raw_path: raw.to_string(),
                kind: "use".to_string(),
            };
            assert!(m.resolve_external_use(&imp, 1, "g").is_none());
        }
        let modimp = RawImport {
            raw_path: "foo".to_string(),
            kind: "mod".to_string(),
        };
        assert!(m.resolve_external_use(&modimp, 1, "g").is_none());
    }

    #[test]
    fn resolve_external_use_resolves_into_foreign_project() {
        let mut by_ident = HashMap::new();
        by_ident.insert("shared".to_string(), vec![entry(20, "src", "gb", true)]);
        let mut fp = HashMap::new();
        let mut p20 = HashMap::new();
        p20.insert("src/api.rs".to_string(), 555i64);
        fp.insert(20, p20);
        let m = map_with(by_ident, fp, HashMap::new());
        let imp = RawImport {
            raw_path: "shared::api::Thing".to_string(),
            kind: "use".to_string(),
        };
        assert_eq!(m.resolve_external_use(&imp, 10, "ga"), Some((555, 20)));
        // Unknown crate (std/tokio) → unresolved.
        let ext = RawImport {
            raw_path: "tokio::runtime::Handle".to_string(),
            kind: "use".to_string(),
        };
        assert!(m.resolve_external_use(&ext, 10, "ga").is_none());
    }
}
