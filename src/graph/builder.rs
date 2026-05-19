//! Builds a CodeGraph from database edge rows.

use super::types::{CodeGraph, EdgeType, EdgeWeight, FileNode};

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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn module_from_path_strips_filename() {
        assert_eq!(module_from_path("a/b/c.rs"), "a/b");
        assert_eq!(module_from_path("main.rs"), "");
        assert_eq!(module_from_path("a/b/"), "a/b");
    }

    #[test]
    fn build_graph_empty_inputs_yields_empty_graph() {
        let g = build_graph(&[], &[]);
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn build_graph_creates_a_node_per_file_meta() {
        let metas = vec![
            FileMetaRow {
                file_id: 1,
                relative_path: "a.rs".into(),
                language: "rust".into(),
            },
            FileMetaRow {
                file_id: 2,
                relative_path: "b.rs".into(),
                language: "rust".into(),
            },
        ];
        let g = build_graph(&[], &metas);
        assert_eq!(g.node_count(), 2);
    }

    #[test]
    fn build_graph_drops_edge_rows_with_missing_target() {
        let metas = vec![FileMetaRow {
            file_id: 1,
            relative_path: "a.rs".into(),
            language: "rust".into(),
        }];
        let edges = vec![GraphEdgeRow {
            source_file_id: 1,
            source_relative_path: "a.rs".into(),
            source_language: "rust".into(),
            target_file_id: None,
            target_relative_path: None,
            target_language: None,
            edge_type: "import".into(),
            weight: 1.0,
        }];
        let g = build_graph(&edges, &metas);
        assert_eq!(g.node_count(), 1);
        assert_eq!(g.edge_count(), 0, "dangling edges must be skipped");
    }

    proptest! {
        /// Node count equals unique file_id count in the (metas ∪ edge endpoints) set.
        #[test]
        fn prop_every_file_meta_becomes_node(
            ids in prop::collection::vec(1i64..100, 1..10usize),
        ) {
            let mut seen = std::collections::HashSet::new();
            let metas: Vec<FileMetaRow> = ids.iter()
                .filter(|id| seen.insert(**id))
                .map(|&id| FileMetaRow {
                    file_id: id,
                    relative_path: format!("f{}.rs", id),
                    language: "rust".into(),
                })
                .collect();
            let g = build_graph(&[], &metas);
            prop_assert_eq!(g.node_count(), metas.len());
        }

        /// `module_from_path("a/b/c.rs")` always strips the last `/…` segment.
        #[test]
        fn prop_module_from_path_strips_last_segment(
            prefix in prop::collection::vec("[a-z]{1,6}", 0..5usize),
            leaf in "[a-z]{1,6}\\.rs",
        ) {
            let path = if prefix.is_empty() {
                leaf.clone()
            } else {
                format!("{}/{}", prefix.join("/"), leaf)
            };
            let m = module_from_path(&path);
            if path.contains('/') {
                prop_assert_eq!(&m, &prefix.join("/"));
                prop_assert!(!m.contains(&leaf));
            } else {
                prop_assert_eq!(&m, "");
            }
        }

        /// Edges whose target_file_id is absent never produce a graph edge.
        #[test]
        fn prop_dangling_edges_never_contribute(
            num_dangling in 1usize..10,
        ) {
            let metas = vec![FileMetaRow {
                file_id: 1,
                relative_path: "a.rs".into(),
                language: "rust".into(),
            }];
            let edges: Vec<GraphEdgeRow> = (0..num_dangling)
                .map(|_| GraphEdgeRow {
                    source_file_id: 1,
                    source_relative_path: "a.rs".into(),
                    source_language: "rust".into(),
                    target_file_id: None,
                    target_relative_path: None,
                    target_language: None,
                    edge_type: "import".into(),
                    weight: 1.0,
                })
                .collect();
            let g = build_graph(&edges, &metas);
            prop_assert_eq!(g.edge_count(), 0);
        }
    }
}
