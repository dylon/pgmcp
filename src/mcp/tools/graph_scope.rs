//! Shared loader for graph tools that operate on either the file import graph
//! or the function call graph (graph-roadmap Phase 2.6). Re-materializes the
//! chosen graph as a uniform **topology-only** `DiGraph<NodeMeta, ()>`, so the
//! generic topology algorithms (articulation points / bridges, HITS, dominator
//! tree) run over both scopes through one code path with one NodeIndex→label
//! mapping. (Weight-reading algorithms keep their own loaders since `()` edges
//! don't carry an `EdgeCost`.)

use std::collections::HashMap;

use petgraph::graph::DiGraph;
use petgraph::visit::EdgeRef;
use rmcp::ErrorData as McpError;

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::tools::fix_helpers::load_import_graph;

/// Display metadata for a graph node. For file scope the `label` is the
/// relative path and `file` is `None`; for function scope the `label` is the
/// function name and `file` is its path.
#[derive(Debug, Clone)]
pub struct NodeMeta {
    pub label: String,
    pub file: Option<String>,
}

impl NodeMeta {
    /// JSON view: `{"file": <path>}` for file scope, or
    /// `{"function": <name>, "file": <path>}` for function scope.
    pub fn to_json(&self) -> serde_json::Value {
        match &self.file {
            Some(path) => serde_json::json!({ "function": self.label, "file": path }),
            None => serde_json::json!({ "file": self.label }),
        }
    }
}

/// Load the project's graph at `scope` ("file" | "function") as a topology-only
/// `DiGraph<NodeMeta, ()>`.
pub async fn load_scoped_graph(
    ctx: &SystemContext,
    project_id: i32,
    scope: &str,
) -> Result<DiGraph<NodeMeta, ()>, McpError> {
    match scope {
        "function" => {
            let pool = ctx
                .db()
                .pool()
                .ok_or_else(|| McpError::internal_error("no database pool".to_string(), None))?;
            let node_rows = queries::list_function_nodes_for_project(pool, project_id)
                .await
                .map_err(|e| {
                    McpError::internal_error(format!("function nodes query failed: {e}"), None)
                })?;
            let raws = queries::list_call_edges_for_project(pool, project_id)
                .await
                .map_err(|e| {
                    McpError::internal_error(format!("call edges query failed: {e}"), None)
                })?;
            let mut g: DiGraph<NodeMeta, ()> = DiGraph::with_capacity(node_rows.len(), raws.len());
            let mut idx_by_sym: HashMap<i64, petgraph::graph::NodeIndex> =
                HashMap::with_capacity(node_rows.len());
            for n in node_rows {
                let ni = g.add_node(NodeMeta {
                    label: n.name,
                    file: Some(n.relative_path),
                });
                idx_by_sym.insert(n.symbol_id, ni);
            }
            for r in &raws {
                if let (Some(s), Some(t)) = (r.source_symbol_id, r.target_symbol_id)
                    && let (Some(&si), Some(&ti)) = (idx_by_sym.get(&s), idx_by_sym.get(&t))
                {
                    g.add_edge(si, ti, ());
                }
            }
            Ok(g)
        }
        "file" => {
            let bundle = load_import_graph(ctx, project_id).await?;
            let src = &bundle.graph.graph;
            let mut g: DiGraph<NodeMeta, ()> =
                DiGraph::with_capacity(src.node_count(), src.edge_count());
            let mut map: HashMap<petgraph::graph::NodeIndex, petgraph::graph::NodeIndex> =
                HashMap::with_capacity(src.node_count());
            for ni in src.node_indices() {
                if let Some(fnode) = src.node_weight(ni) {
                    let nm = g.add_node(NodeMeta {
                        label: fnode.relative_path.clone(),
                        file: None,
                    });
                    map.insert(ni, nm);
                }
            }
            for e in src.edge_references() {
                if let (Some(&s), Some(&t)) = (map.get(&e.source()), map.get(&e.target())) {
                    g.add_edge(s, t, ());
                }
            }
            Ok(g)
        }
        other => Err(McpError::invalid_params(
            format!("Unknown scope '{other}'. Use \"file\" (default) or \"function\"."),
            None,
        )),
    }
}
