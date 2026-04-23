//! Module-level metrics: coupling, cohesion, instability, abstractness.
//!
//! Computes Robert C. Martin's package metrics at a configurable directory depth.

use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use super::{CodeGraph, EdgeType};
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

/// Abstractness detection patterns.
static ABSTRACT_PATTERNS: LazyLock<Vec<(&str, Regex)>> = LazyLock::new(|| {
    vec![
        (
            "rust",
            Regex::new(r"(?m)^\s*(?:pub\s+)?trait\s+\w+").expect("invalid regex"),
        ),
        (
            "java",
            Regex::new(r"(?m)^\s*(?:public\s+)?(?:abstract\s+)?interface\s+\w+")
                .expect("invalid regex"),
        ),
        (
            "java_abstract",
            Regex::new(r"(?m)^\s*(?:public\s+)?abstract\s+class\s+\w+").expect("invalid regex"),
        ),
        (
            "python",
            Regex::new(r"(?m)class\s+\w+\(.*ABC").expect("invalid regex"),
        ),
        (
            "typescript",
            Regex::new(r"(?m)^\s*(?:export\s+)?(?:abstract\s+)?interface\s+\w+")
                .expect("invalid regex"),
        ),
        (
            "go",
            Regex::new(r"(?m)^\s*type\s+\w+\s+interface\s*\{").expect("invalid regex"),
        ),
    ]
});

/// Check if file content contains abstract/interface declarations.
pub fn is_abstract_file(content: &str, language: &str) -> bool {
    for (lang_prefix, re) in ABSTRACT_PATTERNS.iter() {
        let matches_lang = match language {
            "rust" => *lang_prefix == "rust",
            "java" | "kotlin" => lang_prefix.starts_with("java"),
            "python" => *lang_prefix == "python",
            "typescript" | "javascript" => *lang_prefix == "typescript",
            "go" => *lang_prefix == "go",
            _ => false,
        };
        if matches_lang && re.is_match(content) {
            return true;
        }
    }
    false
}

/// Compute module-level metrics from a CodeGraph.
/// `module_depth`: how many directory levels to use for module grouping.
pub fn compute_module_metrics(code_graph: &CodeGraph, module_depth: usize) -> Vec<ModuleMetrics> {
    let graph = &code_graph.graph;

    // Group nodes by module (directory at given depth)
    let mut module_nodes: HashMap<String, Vec<petgraph::graph::NodeIndex>> = HashMap::new();
    for node_idx in graph.node_indices() {
        let node = &graph[node_idx];
        let module = truncate_module(&node.module, module_depth);
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
