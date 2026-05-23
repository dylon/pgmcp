//! Resolved call-edge traversals.
//!
//! Tools that walk `symbol_references` or `code_graph_edges` previously
//! had to handle NULL `target_symbol_id` and bare-name fallback in their
//! own SQL. These helpers concentrate the JOIN against
//! `resolution_kind` / `resolution_confidence` so consumers can choose a
//! precision threshold.

use sqlx::PgPool;
use std::collections::{HashMap, HashSet, VecDeque};

/// One resolved call-shaped edge, with confidence and target path.
#[derive(Debug, Clone)]
pub struct ResolvedEdge {
    pub source_symbol_id: i64,
    pub target_symbol_id: i64,
    pub target_path: Option<String>,
    pub resolution_kind: String,
    pub resolution_confidence: f32,
}

/// Per-row tuple returned by the `resolved_call_edges` query. Aliased
/// so the inferred sqlx return type stays inside clippy's complexity
/// thresholds.
type EdgeRow = (i64, i64, Option<String>, String, Option<f32>);

/// All resolved call edges in a project at or above the given confidence.
/// `min_confidence = 0.0` includes everything (even bare-name); pass
/// `0.95` to restrict to exact_in_file / exact_via_import.
pub async fn resolved_call_edges(
    pool: &PgPool,
    project_id: i32,
    min_confidence: f32,
) -> Result<Vec<ResolvedEdge>, sqlx::Error> {
    let rows: Vec<EdgeRow> = sqlx::query_as(
        "SELECT sr.source_symbol_id, sr.target_symbol_id, sr.target_path,
                COALESCE(sr.resolution_kind, 'unresolved'),
                sr.resolution_confidence
         FROM symbol_references sr
         JOIN indexed_files f ON f.id = sr.source_file_id
         WHERE f.project_id = $1
           AND sr.source_symbol_id IS NOT NULL
           AND sr.target_symbol_id IS NOT NULL
           AND sr.ref_kind = 'call'
           AND COALESCE(sr.resolution_confidence, 0.0) >= $2::real",
    )
    .bind(project_id)
    .bind(min_confidence)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(source_symbol_id, target_symbol_id, target_path, resolution_kind, confidence)| {
                ResolvedEdge {
                    source_symbol_id,
                    target_symbol_id,
                    target_path,
                    resolution_kind,
                    resolution_confidence: confidence.unwrap_or(0.0),
                }
            },
        )
        .collect())
}

/// BFS forward reachability from `seed_symbol_id` over resolved call
/// edges (`exact_in_file`, `exact_via_import` by default). Returns
/// the set of reachable symbol ids paired with their minimum depth.
pub async fn forward_reachability(
    pool: &PgPool,
    seed_symbol_id: i64,
    max_depth: u32,
) -> Result<HashMap<i64, u32>, sqlx::Error> {
    let edges = sqlx::query_as::<_, (i64, i64)>(
        "SELECT source_symbol_id, target_symbol_id
         FROM symbol_references
         WHERE source_symbol_id IS NOT NULL
           AND target_symbol_id IS NOT NULL
           AND resolution_kind IN ('exact_in_file', 'exact_via_import')",
    )
    .fetch_all(pool)
    .await?;
    let mut adj: HashMap<i64, Vec<i64>> = HashMap::new();
    for (s, t) in edges {
        adj.entry(s).or_default().push(t);
    }
    Ok(bfs(&adj, seed_symbol_id, max_depth))
}

/// BFS reverse reachability — who can reach `seed_symbol_id`?
pub async fn reverse_reachability(
    pool: &PgPool,
    seed_symbol_id: i64,
    max_depth: u32,
) -> Result<HashMap<i64, u32>, sqlx::Error> {
    let edges = sqlx::query_as::<_, (i64, i64)>(
        "SELECT source_symbol_id, target_symbol_id
         FROM symbol_references
         WHERE source_symbol_id IS NOT NULL
           AND target_symbol_id IS NOT NULL
           AND resolution_kind IN ('exact_in_file', 'exact_via_import')",
    )
    .fetch_all(pool)
    .await?;
    let mut radj: HashMap<i64, Vec<i64>> = HashMap::new();
    for (s, t) in edges {
        radj.entry(t).or_default().push(s);
    }
    Ok(bfs(&radj, seed_symbol_id, max_depth))
}

fn bfs(adj: &HashMap<i64, Vec<i64>>, seed: i64, max_depth: u32) -> HashMap<i64, u32> {
    let mut visited: HashMap<i64, u32> = HashMap::new();
    let mut queue: VecDeque<(i64, u32)> = VecDeque::new();
    queue.push_back((seed, 0));
    while let Some((node, depth)) = queue.pop_front() {
        if visited.contains_key(&node) {
            continue;
        }
        visited.insert(node, depth);
        if depth >= max_depth {
            continue;
        }
        if let Some(neighbors) = adj.get(&node) {
            for &n in neighbors {
                if !visited.contains_key(&n) {
                    queue.push_back((n, depth + 1));
                }
            }
        }
    }
    visited
}

/// Resolution-confidence distribution for a project — counts per kind.
/// Useful for the engineering-scorecard and resolution-quality reports.
pub async fn resolution_kind_breakdown(
    pool: &PgPool,
    project_id: i32,
) -> Result<HashMap<String, i64>, sqlx::Error> {
    let rows: Vec<(Option<String>, i64)> = sqlx::query_as(
        "SELECT sr.resolution_kind, COUNT(*)::int8
         FROM symbol_references sr
         JOIN indexed_files f ON f.id = sr.source_file_id
         WHERE f.project_id = $1
         GROUP BY sr.resolution_kind
         ORDER BY sr.resolution_kind",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(k, c)| (k.unwrap_or_else(|| "null".into()), c))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bfs_finds_direct_neighbors() {
        let mut adj: HashMap<i64, Vec<i64>> = HashMap::new();
        adj.insert(1, vec![2, 3]);
        adj.insert(2, vec![4]);
        adj.insert(3, vec![]);
        adj.insert(4, vec![5]);
        let reached = bfs(&adj, 1, 10);
        assert_eq!(reached[&1], 0);
        assert_eq!(reached[&2], 1);
        assert_eq!(reached[&3], 1);
        assert_eq!(reached[&4], 2);
        assert_eq!(reached[&5], 3);
    }

    #[test]
    fn bfs_respects_max_depth() {
        let mut adj: HashMap<i64, Vec<i64>> = HashMap::new();
        adj.insert(1, vec![2]);
        adj.insert(2, vec![3]);
        adj.insert(3, vec![4]);
        let reached = bfs(&adj, 1, 2);
        assert!(reached.contains_key(&1));
        assert!(reached.contains_key(&2));
        assert!(reached.contains_key(&3));
        assert!(!reached.contains_key(&4));
    }

    #[test]
    fn bfs_handles_cycles() {
        let mut adj: HashMap<i64, Vec<i64>> = HashMap::new();
        adj.insert(1, vec![2]);
        adj.insert(2, vec![1, 3]);
        adj.insert(3, vec![1]);
        let reached = bfs(&adj, 1, 10);
        assert_eq!(reached.len(), 3);
    }

    #[test]
    fn bfs_seed_alone_when_no_neighbors() {
        let adj: HashMap<i64, Vec<i64>> = HashMap::new();
        let reached = bfs(&adj, 42, 5);
        assert_eq!(reached.len(), 1);
        assert_eq!(reached[&42], 0);
    }

    // Stub: confirm ResolvedEdge / HashSet imports compile. HashSet is
    // re-exported for downstream tools that build adjacency sets.
    #[test]
    fn types_compile() {
        let _: HashSet<i64> = HashSet::new();
        let _ = ResolvedEdge {
            source_symbol_id: 1,
            target_symbol_id: 2,
            target_path: Some("crate::foo".into()),
            resolution_kind: "exact_in_file".into(),
            resolution_confidence: 1.0,
        };
    }
}
