use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::HashMap;

/// Core code graph structure wrapping petgraph DiGraph.
pub struct CodeGraph {
    pub graph: DiGraph<FileNode, EdgeWeight>,
    pub file_id_to_node: HashMap<i64, NodeIndex>,
    pub node_to_file_id: HashMap<NodeIndex, i64>,
}

/// Metadata associated with each file node in the graph.
#[derive(Debug, Clone)]
pub struct FileNode {
    pub file_id: i64,
    pub relative_path: String,
    #[allow(dead_code)]
    // Used in MCP tool handlers via petgraph node weights.
    pub language: String,
    /// Directory path used as module identifier.
    pub module: String,
}

/// Weight and type information for graph edges.
#[derive(Debug, Clone)]
pub struct EdgeWeight {
    pub edge_type: EdgeType,
    pub weight: f64,
}

/// Categorization of dependency relationships.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeType {
    Import,
    CoChange,
    Semantic,
}

impl EdgeType {
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            EdgeType::Import => "import",
            EdgeType::CoChange => "co_change",
            EdgeType::Semantic => "semantic",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "import" => Some(EdgeType::Import),
            "co_change" => Some(EdgeType::CoChange),
            "semantic" => Some(EdgeType::Semantic),
            _ => None,
        }
    }
}

impl Default for CodeGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl CodeGraph {
    pub fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            file_id_to_node: HashMap::new(),
            node_to_file_id: HashMap::new(),
        }
    }

    /// Get or insert a node for the given file.
    pub fn ensure_node(&mut self, node: FileNode) -> NodeIndex {
        if let Some(&idx) = self.file_id_to_node.get(&node.file_id) {
            return idx;
        }
        let file_id = node.file_id;
        let idx = self.graph.add_node(node);
        self.file_id_to_node.insert(file_id, idx);
        self.node_to_file_id.insert(idx, file_id);
        idx
    }

    /// Add a directed edge between two file nodes.
    pub fn add_edge(&mut self, source: NodeIndex, target: NodeIndex, weight: EdgeWeight) {
        self.graph.add_edge(source, target, weight);
    }

    /// Number of nodes in the graph.
    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    /// Number of edges in the graph.
    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }
}
