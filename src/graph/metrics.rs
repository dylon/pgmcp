//! Module-level metrics: coupling, cohesion, instability, abstractness.
//!
//! Computes Robert C. Martin's package metrics at a configurable directory depth.

use std::collections::{HashMap, HashSet};

use super::cargo_layout::CrateLayout;
use super::types::{CodeGraph, EdgeType, FileNode};
use petgraph::Direction;

/// Metrics for a single module (directory grouping).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModuleMetrics {
    pub module_path: String,
    pub file_count: usize,
    /// Afferent coupling: number of external files importing files in this module.
    pub afferent_coupling: usize,
    /// Efferent coupling: number of external files this module's files import.
    pub efferent_coupling: usize,
    /// Instability I = Ce / (Ca + Ce). 0 = maximally stable, 1 = maximally unstable.
    pub instability: f64,
    /// Abstractness A = abstract_files / total_files.
    pub abstractness: f64,
    /// Distance from main sequence D* = |A + I - 1|.
    pub distance_from_main_sequence: f64,
    /// Topic cohesion: 1 - normalized Shannon entropy of topic distribution.
    pub cohesion: Option<f64>,
    /// Files in this module.
    pub files: Vec<String>,
}

/// How a file is assigned to a module bucket for Martin's package metrics.
///
/// Two modes:
/// - [`ModuleBucketer::by_depth`] — the legacy directory-depth bucketing (first
///   `depth` path segments of the file's directory). Correct for non-cargo
///   languages and the internal heuristics that don't carry a crate layout.
/// - [`ModuleBucketer::crate_aware`] — Rust files are bucketed by their **Cargo
///   crate** (from [`CrateLayout`]), the true package unit of a workspace, so
///   an edge counts toward Ca/Ce iff it crosses a crate boundary. Non-Rust /
///   crate-less files fall back to directory depth. This is what lifts
///   single-crate-per-directory Rust repos out of the `Ca=Ce=0` degeneracy that
///   fixed depth-2 produced (everything collapsing to one `src` bucket).
///
/// Crate buckets are keyed `crate:<ident>[/<subdir>]`; the `crate:` namespace
/// prefix never collides with a directory bucket (which contains no `:`), so
/// crate-aware and fallback buckets coexist unambiguously in one result set.
pub struct ModuleBucketer<'a> {
    layout: Option<&'a CrateLayout>,
    fallback_depth: usize,
    intra_crate_depth: usize,
}

impl<'a> ModuleBucketer<'a> {
    /// Legacy directory-depth bucketing (back-compat; the body of the old
    /// `compute_module_metrics(graph, depth)`).
    pub fn by_depth(depth: usize) -> Self {
        Self {
            layout: None,
            fallback_depth: depth,
            intra_crate_depth: 0,
        }
    }

    /// Crate-aware bucketing: Rust files bucket by crate; everything else by
    /// `fallback_depth`. `intra_crate_depth` > 0 splits a crate into
    /// sub-modules that many directory levels deep (0 = whole crate is one
    /// bucket — the canonical package unit).
    pub fn crate_aware(
        layout: &'a CrateLayout,
        fallback_depth: usize,
        intra_crate_depth: usize,
    ) -> Self {
        Self {
            layout: Some(layout),
            fallback_depth,
            intra_crate_depth,
        }
    }

    /// The bucket key for one file node.
    fn bucket(&self, node: &FileNode) -> String {
        if let Some(layout) = self.layout
            && node.language == "rust"
            && let Some((ident, src_dir)) = layout.crate_of_path(&node.relative_path)
        {
            if self.intra_crate_depth == 0 {
                return format!("crate:{ident}");
            }
            // Directory of the file relative to the crate src root, truncated.
            let within = node
                .relative_path
                .strip_prefix(src_dir)
                .map(|s| s.trim_start_matches('/'))
                .unwrap_or("");
            let sub = truncate_module(directory_of(within), self.intra_crate_depth);
            return if sub.is_empty() {
                format!("crate:{ident}")
            } else {
                format!("crate:{ident}/{sub}")
            };
        }
        truncate_module(&node.module, self.fallback_depth)
    }
}

/// Compute module-level metrics from a CodeGraph using directory-depth buckets.
/// `module_depth`: how many directory levels to use for module grouping.
/// Back-compat shim over [`compute_module_metrics_with`]; existing callers are
/// unchanged.
pub fn compute_module_metrics(code_graph: &CodeGraph, module_depth: usize) -> Vec<ModuleMetrics> {
    compute_module_metrics_with(code_graph, &ModuleBucketer::by_depth(module_depth))
}

/// Compute module-level metrics from a CodeGraph using an explicit bucketer
/// (directory-depth or crate-aware). See [`ModuleBucketer`].
pub fn compute_module_metrics_with(
    code_graph: &CodeGraph,
    bucketer: &ModuleBucketer,
) -> Vec<ModuleMetrics> {
    let graph = &code_graph.graph;

    // Group nodes by module bucket.
    let mut module_nodes: HashMap<String, Vec<petgraph::graph::NodeIndex>> = HashMap::new();
    for node_idx in graph.node_indices() {
        let node = &graph[node_idx];
        let module = bucketer.bucket(node);
        module_nodes.entry(module).or_default().push(node_idx);
    }

    // For each module, compute Ca, Ce
    let mut results = Vec::new();

    for (module_path, nodes) in &module_nodes {
        let node_set: HashSet<petgraph::graph::NodeIndex> = nodes.iter().copied().collect();
        let mut ca_set: HashSet<petgraph::graph::NodeIndex> = HashSet::new(); // external nodes importing us
        let mut ce_set: HashSet<petgraph::graph::NodeIndex> = HashSet::new(); // external nodes we import

        for &node in nodes {
            // Incoming edges from outside this module -> Ca
            for source in graph.neighbors_directed(node, Direction::Incoming) {
                if !node_set.contains(&source) {
                    // Check if it's an import edge
                    if graph
                        .edges_connecting(source, node)
                        .any(|e| e.weight().edge_type == EdgeType::Import)
                    {
                        ca_set.insert(source);
                    }
                }
            }

            // Outgoing edges to outside this module -> Ce
            for target in graph.neighbors_directed(node, Direction::Outgoing) {
                if !node_set.contains(&target)
                    && graph
                        .edges_connecting(node, target)
                        .any(|e| e.weight().edge_type == EdgeType::Import)
                {
                    ce_set.insert(target);
                }
            }
        }

        let ca = ca_set.len();
        let ce = ce_set.len();
        let instability = if ca + ce > 0 {
            ce as f64 / (ca + ce) as f64
        } else {
            0.0
        };

        let files: Vec<String> = nodes
            .iter()
            .map(|&n| graph[n].relative_path.clone())
            .collect();

        results.push(ModuleMetrics {
            module_path: module_path.clone(),
            file_count: nodes.len(),
            afferent_coupling: ca,
            efferent_coupling: ce,
            instability,
            abstractness: 0.0, // Will be computed separately with content access
            distance_from_main_sequence: instability.abs(), // Placeholder until abstractness is set
            cohesion: None,
            files,
        });
    }

    results
}

/// The directory portion of a relative path (everything before the last `/`),
/// or `""` for a top-level file. Used to derive a file's intra-crate sub-module.
fn directory_of(relative_path: &str) -> &str {
    match relative_path.rfind('/') {
        Some(pos) => &relative_path[..pos],
        None => "",
    }
}

/// Truncate a module path to the specified depth.
fn truncate_module(module: &str, depth: usize) -> String {
    if depth == 0 || module.is_empty() {
        return String::new();
    }
    let parts: Vec<&str> = module.split('/').take(depth).collect();
    parts.join("/")
}

/// Update abstractness for modules given file content access.
/// `file_abstractions`: map of relative_path -> (is_abstract, language)
pub fn update_abstractness(
    metrics: &mut [ModuleMetrics],
    file_abstractions: &HashMap<String, bool>,
) {
    for module in metrics.iter_mut() {
        let total = module.file_count as f64;
        if total == 0.0 {
            module.abstractness = 0.0;
        } else {
            let abstract_count = module
                .files
                .iter()
                .filter(|f| file_abstractions.get(f.as_str()).copied().unwrap_or(false))
                .count() as f64;
            module.abstractness = abstract_count / total;
        }
        module.distance_from_main_sequence = (module.abstractness + module.instability - 1.0).abs();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::types::{CodeGraph, EdgeType, EdgeWeight, FileNode};
    use proptest::prelude::*;

    fn make_graph(files: Vec<(i64, &str, &str)>, edges: Vec<(i64, i64)>) -> CodeGraph {
        let mut cg = CodeGraph::new();
        for (file_id, path, language) in files {
            let module = path
                .rsplit_once('/')
                .map(|(d, _)| d)
                .unwrap_or("")
                .to_string();
            cg.ensure_node(FileNode {
                file_id,
                relative_path: path.to_string(),
                language: language.to_string(),
                module,
            });
        }
        for (src, dst) in edges {
            let s = cg.file_id_to_node[&src];
            let d = cg.file_id_to_node[&dst];
            cg.add_edge(
                s,
                d,
                EdgeWeight {
                    weight: 1.0,
                    edge_type: EdgeType::Import,
                },
            );
        }
        cg
    }

    fn node(path: &str, lang: &str) -> FileNode {
        FileNode {
            file_id: 0,
            relative_path: path.to_string(),
            language: lang.to_string(),
            module: path
                .rsplit_once('/')
                .map(|(d, _)| d)
                .unwrap_or("")
                .to_string(),
        }
    }

    #[test]
    fn crate_aware_bucketer_keys_by_crate() {
        let layout = CrateLayout::from_map(
            [
                ("mettail_prattail".to_string(), "prattail/src".to_string()),
                ("mettail_ast".to_string(), "ast/src".to_string()),
            ]
            .into_iter()
            .collect(),
        );
        let b0 = ModuleBucketer::crate_aware(&layout, 2, 0);
        assert_eq!(
            b0.bucket(&node("prattail/src/wpda/walker.rs", "rust")),
            "crate:mettail_prattail"
        );
        assert_eq!(
            b0.bucket(&node("ast/src/language.rs", "rust")),
            "crate:mettail_ast"
        );
        // intra-crate sub-depth splits within a crate
        let b1 = ModuleBucketer::crate_aware(&layout, 2, 1);
        assert_eq!(
            b1.bucket(&node("prattail/src/wpda/walker.rs", "rust")),
            "crate:mettail_prattail/wpda"
        );
        // non-rust and outside-any-crate files fall back to directory depth
        assert_eq!(b0.bucket(&node("docs/adr/001.md", "markdown")), "docs/adr");
        assert_eq!(b0.bucket(&node("prattail/build.rs", "rust")), "prattail");
    }

    #[test]
    fn by_depth_bucketer_matches_truncate_module() {
        let b = ModuleBucketer::by_depth(2);
        let n = node("src/mcp/tools/foo.rs", "rust");
        assert_eq!(b.bucket(&n), truncate_module(&n.module, 2));
    }

    #[test]
    fn compute_module_metrics_with_by_depth_equals_legacy() {
        // The back-compat shim must be a faithful refactor of the old fn.
        let cg = make_graph(
            vec![(1, "a/x.rs", "rust"), (2, "b/y.rs", "rust")],
            vec![(1, 2)],
        );
        let legacy = compute_module_metrics(&cg, 2);
        let viaw = compute_module_metrics_with(&cg, &ModuleBucketer::by_depth(2));
        assert_eq!(legacy.len(), viaw.len());
        // a→b is a cross-module edge ⇒ each module has exactly one Ca/Ce side.
        let total_ce: usize = viaw.iter().map(|m| m.efferent_coupling).sum();
        assert_eq!(total_ce, 1);
    }

    #[test]
    fn crate_buckets_make_cross_crate_edges_inter_module() {
        // Two crates, one inter-crate import: with crate buckets the edge counts
        // toward Ca/Ce; with single-`src` depth-2 it would have collapsed.
        let layout = CrateLayout::from_map(
            [
                ("foo".to_string(), "foo/src".to_string()),
                ("bar".to_string(), "bar/src".to_string()),
            ]
            .into_iter()
            .collect(),
        );
        let cg = make_graph(
            vec![(1, "foo/src/a.rs", "rust"), (2, "bar/src/b.rs", "rust")],
            vec![(1, 2)],
        );
        let mm = compute_module_metrics_with(&cg, &ModuleBucketer::crate_aware(&layout, 2, 0));
        let foo = mm
            .iter()
            .find(|m| m.module_path == "crate:foo")
            .expect("foo bucket");
        let bar = mm
            .iter()
            .find(|m| m.module_path == "crate:bar")
            .expect("bar bucket");
        assert_eq!((foo.efferent_coupling, foo.afferent_coupling), (1, 0));
        assert_eq!((bar.afferent_coupling, bar.efferent_coupling), (1, 0));
    }

    #[test]
    fn truncate_module_honors_depth() {
        assert_eq!(truncate_module("a/b/c/d", 0), "");
        assert_eq!(truncate_module("a/b/c/d", 1), "a");
        assert_eq!(truncate_module("a/b/c/d", 2), "a/b");
        assert_eq!(truncate_module("a/b/c/d", 10), "a/b/c/d");
        assert_eq!(truncate_module("", 3), "");
    }

    #[test]
    fn compute_metrics_two_modules_simple() {
        // a/foo.rs --(import)--> b/bar.rs ; two modules, Ce=1, Ca=1
        let graph = make_graph(
            vec![(1, "a/foo.rs", "rust"), (2, "b/bar.rs", "rust")],
            vec![(1, 2)],
        );
        let metrics = compute_module_metrics(&graph, 1);
        assert_eq!(metrics.len(), 2);
        let a = metrics.iter().find(|m| m.module_path == "a").expect("a");
        let b = metrics.iter().find(|m| m.module_path == "b").expect("b");
        assert_eq!(a.efferent_coupling, 1);
        assert_eq!(a.afferent_coupling, 0);
        assert_eq!(b.efferent_coupling, 0);
        assert_eq!(b.afferent_coupling, 1);
        assert_eq!(a.instability, 1.0);
        assert_eq!(b.instability, 0.0);
    }

    #[test]
    fn update_abstractness_writes_distance_from_main_sequence() {
        let mut metrics = vec![ModuleMetrics {
            module_path: "m".into(),
            file_count: 4,
            afferent_coupling: 1,
            efferent_coupling: 3,
            instability: 0.75,
            abstractness: 0.0,
            distance_from_main_sequence: 0.0,
            cohesion: None,
            files: vec!["a".into(), "b".into(), "c".into(), "d".into()],
        }];
        let mut abs = HashMap::new();
        abs.insert("a".to_string(), true);
        update_abstractness(&mut metrics, &abs);
        assert!((metrics[0].abstractness - 0.25).abs() < 1e-9);
        // D* = |A + I - 1| = |0.25 + 0.75 - 1| = 0.0
        assert!(metrics[0].distance_from_main_sequence < 1e-9);
    }

    // ========================================================================
    // Property tests
    // ========================================================================

    proptest! {
        /// Ca and Ce are always non-negative (usize counts by definition).
        /// Instability is in [0, 1].
        #[test]
        fn prop_instability_in_unit_interval(
            num_modules in 2usize..5,
            edges_per_pair in 0usize..3,
        ) {
            // Build num_modules directories, 2 files each. Connect them in a
            // chain with `edges_per_pair` edges per adjacent module pair.
            let mut files = Vec::new();
            let mut next_id = 1i64;
            for m in 0..num_modules {
                for f in 0..2 {
                    files.push((next_id, format!("m{}/f{}.rs", m, f)));
                    next_id += 1;
                }
            }
            let files_refs: Vec<(i64, &str, &str)> = files.iter()
                .map(|(id, p)| (*id, p.as_str(), "rust"))
                .collect();
            let mut edges = Vec::new();
            for m in 0..num_modules.saturating_sub(1) {
                for _ in 0..edges_per_pair {
                    let src_id = (m * 2 + 1) as i64;
                    let dst_id = ((m + 1) * 2 + 1) as i64;
                    edges.push((src_id, dst_id));
                }
            }
            let graph = make_graph(files_refs, edges);
            let metrics = compute_module_metrics(&graph, 1);
            for m in &metrics {
                prop_assert!((0.0..=1.0).contains(&m.instability),
                    "instability {} outside [0, 1]", m.instability);
                prop_assert!((0.0..=1.0).contains(&m.abstractness),
                    "abstractness {} outside [0, 1]", m.abstractness);
            }
        }

        /// D* = |A + I − 1| is always in [0, 1].
        #[test]
        fn prop_distance_from_main_sequence_in_unit_interval(
            instability in 0.0f64..=1.0,
            abstractness in 0.0f64..=1.0,
        ) {
            let mut m = vec![ModuleMetrics {
                module_path: "m".into(),
                file_count: 1,
                afferent_coupling: 0,
                efferent_coupling: 0,
                instability,
                abstractness: 0.0,
                distance_from_main_sequence: 0.0,
                cohesion: None,
                files: vec!["f".into()],
            }];
            let mut abs = HashMap::new();
            abs.insert("f".to_string(), abstractness >= 0.5);
            update_abstractness(&mut m, &abs);
            let d_star = m[0].distance_from_main_sequence;
            prop_assert!((0.0..=1.0).contains(&d_star),
                "D* = {} outside [0, 1]", d_star);
        }

        /// Every input file appears in exactly one module bucket.
        #[test]
        fn prop_metrics_partition_all_files(
            num_files in 1usize..20,
            module_depth in 1usize..4,
        ) {
            let files: Vec<(i64, String)> = (0..num_files)
                .map(|i| (i as i64 + 1, format!("a/b/c/file{}.rs", i)))
                .collect();
            let files_refs: Vec<(i64, &str, &str)> = files.iter()
                .map(|(id, p)| (*id, p.as_str(), "rust"))
                .collect();
            let graph = make_graph(files_refs, vec![]);
            let metrics = compute_module_metrics(&graph, module_depth);
            let total_files: usize = metrics.iter().map(|m| m.file_count).sum();
            prop_assert_eq!(total_files, num_files);
        }

        /// truncate_module never produces more than `depth` slashes + 1 segment.
        #[test]
        fn prop_truncate_module_never_exceeds_depth(
            parts in prop::collection::vec("[a-z]{1,8}", 0..10usize),
            depth in 0usize..6,
        ) {
            let module = parts.join("/");
            let truncated = truncate_module(&module, depth);
            if depth == 0 {
                prop_assert_eq!(&truncated, "");
            } else if !truncated.is_empty() {
                let seg_count = truncated.split('/').count();
                prop_assert!(seg_count <= depth,
                    "truncate_module({:?}, {}) = {:?} has {} segments",
                    module, depth, truncated, seg_count);
            }
        }
    }
}
