//! Regression test for Phase 6 (parallelized Brandes betweenness).
//!
//! Asserts that `betweenness_centrality_parallel` returns numerically-
//! equivalent results to the sequential `betweenness_centrality` on a
//! synthetic directed graph. Because both paths accumulate identical
//! contributions per source, the result must be bit-exact up to the
//! summation-order tolerance (we use 1e-9 max-absolute-error).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use petgraph::graph::DiGraph;
use pgmcp::graph::algorithms::{betweenness_centrality, betweenness_centrality_parallel};
use pgmcp::graph::{EdgeType, EdgeWeight, FileNode};
use pgmcp::work_pool::pool::WorkPool;

/// Build a small directed graph of N nodes connected as a ring +
/// random "shortcut" edges, which gives Brandes a non-trivial set of
/// shortest paths to enumerate.
fn build_test_graph(n: usize, shortcut_seed: u64) -> DiGraph<FileNode, EdgeWeight> {
    let mut graph = DiGraph::<FileNode, EdgeWeight>::new();
    let nodes: Vec<_> = (0..n)
        .map(|i| {
            graph.add_node(FileNode {
                file_id: i as i64,
                relative_path: format!("f{i}.rs"),
                language: "rust".into(),
                module: "src".into(),
            })
        })
        .collect();

    // Ring of forward edges.
    for i in 0..n {
        let src = nodes[i];
        let dst = nodes[(i + 1) % n];
        graph.add_edge(
            src,
            dst,
            EdgeWeight {
                edge_type: EdgeType::Import,
                weight: 1.0,
            },
        );
    }

    // Deterministic "shortcut" edges via a linear-congruential RNG so
    // the test is reproducible without pulling in `rand`.
    let mut s = shortcut_seed.wrapping_add(1);
    let shortcut_count = n / 4;
    for _ in 0..shortcut_count {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let i = (s >> 33) as usize % n;
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let j = (s >> 33) as usize % n;
        if i != j {
            graph.add_edge(
                nodes[i],
                nodes[j],
                EdgeWeight {
                    edge_type: EdgeType::Import,
                    weight: 1.0,
                },
            );
        }
    }

    graph
}

#[test]
fn parallel_matches_sequential_on_small_ring() {
    let graph = build_test_graph(40, 42);
    let shutdown = Arc::new(AtomicBool::new(false));
    let pool = Arc::new(WorkPool::new(2, 4, 4, Arc::clone(&shutdown)));

    let seq = betweenness_centrality(&graph);
    let par = betweenness_centrality_parallel(&graph, &pool, None);

    assert_eq!(
        seq.len(),
        par.len(),
        "both methods report the same set of nodes"
    );

    let mut max_abs_diff = 0.0f64;
    for (node_idx, seq_val) in &seq {
        let par_val = par
            .get(node_idx)
            .copied()
            .expect("parallel result missing a node from sequential result");
        let diff = (seq_val - par_val).abs();
        if diff > max_abs_diff {
            max_abs_diff = diff;
        }
    }

    // Summation order in the parallel path is non-deterministic across
    // workers, so equivalence is up to floating-point reduction noise.
    assert!(
        max_abs_diff <= 1e-9,
        "max abs diff between sequential and parallel centrality is {max_abs_diff}, expected <= 1e-9"
    );

    shutdown.store(true, std::sync::atomic::Ordering::Release);
}

#[test]
fn parallel_handles_trivial_graphs_without_panic() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let pool = Arc::new(WorkPool::new(1, 2, 2, Arc::clone(&shutdown)));

    // n == 0: empty graph -> empty map.
    let g0 = DiGraph::<FileNode, EdgeWeight>::new();
    let r0 = betweenness_centrality_parallel(&g0, &pool, None);
    assert!(r0.is_empty(), "empty graph yields empty centrality");

    // n == 2: degenerate (n-1)*(n-2) == 0; both methods short-circuit.
    let mut g2 = DiGraph::<FileNode, EdgeWeight>::new();
    let a = g2.add_node(FileNode {
        file_id: 1,
        relative_path: "a.rs".into(),
        language: "rust".into(),
        module: "src".into(),
    });
    let b = g2.add_node(FileNode {
        file_id: 2,
        relative_path: "b.rs".into(),
        language: "rust".into(),
        module: "src".into(),
    });
    g2.add_edge(
        a,
        b,
        EdgeWeight {
            edge_type: EdgeType::Import,
            weight: 1.0,
        },
    );
    let r2 = betweenness_centrality_parallel(&g2, &pool, None);
    assert_eq!(r2.len(), 2, "two-node graph yields entries for both nodes");
    for v in r2.values() {
        assert_eq!(*v, 0.0, "trivial graph centrality must be exactly 0.0");
    }

    shutdown.store(true, std::sync::atomic::Ordering::Release);
}
