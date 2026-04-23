//! Builds a CodeGraph from database edge rows.

use super::{CodeGraph, EdgeType, EdgeWeight, FileNode};

/// A row from the code_graph_edges table joined with file metadata.
#[derive(Debug, Clone)]
pub struct GraphEdgeRow {
    pub source_file_id: i64,
    pub source_relative_path: String,
    pub source_language: String,
    pub target_file_id: Option<i64>,
    pub target_relative_path: Option<String>,
    pub target_language: Option<String>,
    pub edge_type: String,
    pub weight: f64,
}

/// File metadata row for populating graph nodes.
#[derive(Debug, Clone)]
pub struct FileMetaRow {
    pub file_id: i64,
    pub relative_path: String,
    pub language: String,
}

/// Extract the module (directory) from a relative path.
fn module_from_path(relative_path: &str) -> String {
    match relative_path.rfind('/') {
        Some(pos) => relative_path[..pos].to_string(),
        None => String::new(),
    }
}

/// Build a CodeGraph from edge rows.
/// The `file_metas` parameter provides metadata for all files in the project
/// (some may not have edges but should still be nodes).
pub fn build_graph(edges: &[GraphEdgeRow], file_metas: &[FileMetaRow]) -> CodeGraph {
    let mut graph = CodeGraph::new();

    // Add all files as nodes first
    for meta in file_metas {
        graph.ensure_node(FileNode {
            file_id: meta.file_id,
            relative_path: meta.relative_path.clone(),
            language: meta.language.clone(),
            module: module_from_path(&meta.relative_path),
        });
    }

    // Add edges
    for edge in edges {
        let source_idx = graph.ensure_node(FileNode {
            file_id: edge.source_file_id,
            relative_path: edge.source_relative_path.clone(),
            language: edge.source_language.clone(),
            module: module_from_path(&edge.source_relative_path),
        });

        if let (Some(target_id), Some(target_path), Some(target_lang)) = (
            edge.target_file_id,
            edge.target_relative_path.as_ref(),
            edge.target_language.as_ref(),
        ) {
            let target_idx = graph.ensure_node(FileNode {
                file_id: target_id,
                relative_path: target_path.clone(),
                language: target_lang.clone(),
                module: module_from_path(target_path),
            });

            let edge_type = EdgeType::from_str(&edge.edge_type).unwrap_or(EdgeType::Import);
            graph.add_edge(
                source_idx,
                target_idx,
                EdgeWeight {
                    edge_type,
                    weight: edge.weight,
                },
            );
        }
    }

    graph
}
