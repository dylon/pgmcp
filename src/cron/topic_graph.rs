//! Graph-hybrid topic clustering (Phase 2, Track A) — the novel engine.
//!
//! ## Idea
//!
//! The embedding-clustering engine collapsed because it clustered raw 1024-d
//! vectors (distance concentration → uniform fuzzy memberships). Meanwhile the
//! *code graph* is healthy: `code_graph_edges` already holds ~422k semantic
//! (embedding-kNN), ~130k import, and ~8k co-change edges, all project-scoped,
//! and the Louvain community detector over it produces well-separated
//! communities with a real modularity score.
//!
//! Track A reuses that healthy subsystem to *repair* topic modeling: it fuses
//! the three edge types into one weighted file graph, runs
//! [`louvain_communities`], and treats each community as a topic — labeling it
//! with the same c-TF-IDF used by the embedding tracks. Because Louvain operates
//! on the graph (never on a 1024-d distance), it sidesteps the
//! curse-of-dimensionality entirely, and modularity gives a free, principled
//! quality signal.
//!
//! Topics here are **file-granular** (every chunk of a file inherits the file's
//! community). That is coarser than per-chunk fuzzy topics but is exactly the
//! "what is this module about" granularity that aids navigation, and it makes
//! the assignment a clean hard partition (no 198-topics-per-chunk smearing).
//!
//! This is the SOTA "community detection as topic model" approach
//! (Leiden/Louvain over a fused semantic+structural graph); the Leiden
//! refinement step is a future upgrade over the bundled Louvain.

use std::collections::{HashMap, HashSet};

use ndarray::Array2;
use petgraph::graph::{DiGraph, NodeIndex};
use tracing::info;

use crate::cron::topic_clustering::{
    ClusteringSummary, TopicFileEntry, TopicKeyword, TopicResult, compute_ctf_idf,
};
use crate::db::DbClient;
use crate::db::queries::ChunkEmbeddingRow;
use crate::graph::algorithms::louvain_communities;
use crate::graph::types::{EdgeType, EdgeWeight};
use crate::quality::topic_metrics::{DEFAULT_COHERENCE_TOP_N, TopicMetrics};

/// One file-level edge from `code_graph_edges`, narrowed to what the fusion
/// needs.
#[derive(Debug, Clone)]
pub struct GraphEdgeLite {
    pub src_file: i64,
    pub dst_file: i64,
    pub edge_type: EdgeType,
    pub weight: f64,
}

/// Index into the `[semantic, import, co_change]` fusion-weight array.
fn edge_type_idx(t: EdgeType) -> usize {
    match t {
        EdgeType::Semantic => 0,
        EdgeType::Import => 1,
        EdgeType::CoChange => 2,
    }
}

/// Load a project's file-level graph edges (`code_graph_edges`) for the graph
/// track. Only edges with both endpoints non-null are returned.
pub async fn load_project_graph_edges(
    db: &dyn DbClient,
    project_id: i32,
) -> Result<Vec<GraphEdgeLite>, sqlx::Error> {
    let pool = db
        .pool()
        .expect("load_project_graph_edges requires a real &PgPool");
    #[derive(sqlx::FromRow)]
    struct Row {
        source_file_id: Option<i64>,
        target_file_id: Option<i64>,
        edge_type: String,
        weight: Option<f64>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT source_file_id, target_file_id, edge_type, weight
         FROM code_graph_edges
         WHERE project_id = $1
           AND source_file_id IS NOT NULL
           AND target_file_id IS NOT NULL",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let (src, dst) = (r.source_file_id?, r.target_file_id?);
            let et = EdgeType::from_str(&r.edge_type)?;
            Some(GraphEdgeLite {
                src_file: src,
                dst_file: dst,
                edge_type: et,
                weight: r.weight.unwrap_or(1.0),
            })
        })
        .collect())
}

/// Cluster a project's chunks into topics via community detection over the
/// fused file graph.
///
/// `edge_weights` is `[semantic, import, co_change]`; `resolution` is the
/// Louvain resolution. Communities with fewer than `min_cluster_size` chunks
/// are dropped to noise.
#[allow(clippy::too_many_arguments)]
pub fn cluster_graph(
    rows: &[ChunkEmbeddingRow],
    edges: &[GraphEdgeLite],
    edge_weights: [f64; 3],
    resolution: f64,
    min_cluster_size: usize,
    label_top_k: usize,
    scope: &str,
) -> ClusteringSummary {
    let n = rows.len();
    if n == 0 {
        return empty_summary(scope);
    }

    // Build the L2-normalized embedding matrix (for cohesion / centroid /
    // representative — NOT for clustering; clustering is on the graph).
    let d = rows[0].embedding.len();
    let mut data = Array2::<f32>::zeros((n, d));
    for (i, row) in rows.iter().enumerate() {
        for (j, &v) in row.embedding.iter().enumerate() {
            data[[i, j]] = v;
        }
        let norm: f32 = data.row(i).dot(&data.row(i)).sqrt();
        if norm > 1e-12 {
            data.row_mut(i).mapv_inplace(|x| x / norm);
        }
    }

    // file_id → chunk indices (a file's chunks all share its community).
    let mut file_chunks: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        file_chunks.entry(row.file_id).or_default().push(i);
    }
    let present: HashSet<i64> = file_chunks.keys().copied().collect();

    // Build the fused file graph. Every file-with-chunks becomes a node (so
    // edgeless files land in singleton communities → dropped to noise below).
    let mut graph: DiGraph<i64, EdgeWeight> = DiGraph::new();
    let mut node_of: HashMap<i64, NodeIndex> = HashMap::new();
    for &fid in &present {
        let idx = graph.add_node(fid);
        node_of.insert(fid, idx);
    }
    let mut edges_added = 0usize;
    for e in edges {
        if !present.contains(&e.src_file) || !present.contains(&e.dst_file) {
            continue;
        }
        if e.src_file == e.dst_file {
            continue;
        }
        let w = edge_weights[edge_type_idx(e.edge_type)] * e.weight;
        if w <= 0.0 {
            continue;
        }
        let (sa, sb) = (node_of[&e.src_file], node_of[&e.dst_file]);
        graph.add_edge(
            sa,
            sb,
            EdgeWeight {
                edge_type: e.edge_type,
                weight: w,
            },
        );
        edges_added += 1;
    }

    let louvain = louvain_communities(&graph, resolution);
    info!(
        scope,
        files = present.len(),
        edges = edges_added,
        communities = louvain.num_communities,
        modularity = format!("{:.4}", louvain.modularity),
        "graph track: Louvain complete"
    );

    // community id → chunk indices.
    let mut comm_chunks: HashMap<usize, Vec<usize>> = HashMap::new();
    for (&fid, chunk_idxs) in &file_chunks {
        if let Some(&node) = node_of.get(&fid)
            && let Some(&comm) = louvain.communities.get(&node)
        {
            comm_chunks.entry(comm).or_default().extend(chunk_idxs);
        }
    }

    // Keep communities with >= min_cluster_size chunks; others → noise.
    let mut kept: Vec<(usize, Vec<usize>)> = comm_chunks
        .into_iter()
        .filter(|(_, idxs)| idxs.len() >= min_cluster_size.max(1))
        .collect();
    // Stable order: largest first.
    kept.sort_by_key(|(_, idxs)| std::cmp::Reverse(idxs.len()));
    let k = kept.len();
    let noise_chunks = n - kept.iter().map(|(_, v)| v.len()).sum::<usize>();

    if k == 0 {
        return ClusteringSummary {
            scope: scope.to_string(),
            chunks_analyzed: n,
            topics_found: 0,
            noise_chunks,
            num_clusters: 0,
            fuzziness: 0.0,
            converged: true,
            iterations: 1,
            topics: Vec::new(),
            metrics: None,
        };
    }

    // One-hot membership matrix (n × k) for c-TF-IDF (hard partition).
    let mut membership = Array2::<f32>::zeros((n, k));
    for (topic_idx, (_comm, idxs)) in kept.iter().enumerate() {
        for &ci in idxs {
            membership[[ci, topic_idx]] = 1.0;
        }
    }
    let contents: Vec<&str> = rows.iter().map(|r| r.content.as_str()).collect();
    let keyword_sets = compute_ctf_idf(&contents, &membership, label_top_k);

    // Assemble topics.
    let mut topics: Vec<TopicResult> = Vec::with_capacity(k);
    for (topic_idx, (_comm, member_indices)) in kept.iter().enumerate() {
        let chunk_ids: Vec<i64> = member_indices.iter().map(|&i| rows[i].chunk_id).collect();
        let file_ids: Vec<i64> = member_indices
            .iter()
            .map(|&i| rows[i].file_id)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let project_names: Vec<String> = member_indices
            .iter()
            .map(|&i| rows[i].project_name.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        let avg_sim = cohesion(&data, member_indices);
        let representative_chunk_id = representative(&data, &chunk_ids, member_indices);
        let representative_snippet = rows
            .iter()
            .find(|r| r.chunk_id == representative_chunk_id)
            .map(|r| {
                if r.content.len() > 500 {
                    format!("{}...", &r.content[..r.content.floor_char_boundary(500)])
                } else {
                    r.content.clone()
                }
            })
            .unwrap_or_default();

        // top files (by chunk count in this topic).
        let mut file_counts: HashMap<(&str, &str), i32> = HashMap::new();
        for &i in member_indices {
            *file_counts
                .entry((rows[i].path.as_str(), rows[i].project_name.as_str()))
                .or_insert(0) += 1;
        }
        let mut top_files: Vec<TopicFileEntry> = file_counts
            .into_iter()
            .map(|((path, project), c)| TopicFileEntry {
                path: path.to_string(),
                project: project.to_string(),
                chunks_in_topic: c,
            })
            .collect();
        top_files.sort_by_key(|b| std::cmp::Reverse(b.chunks_in_topic));

        // 1024-d centroid (mean of member embeddings, L2-normalized) so it stays
        // compatible with the hierarchy + warm-start, which expect embedding-dim.
        let centroid = mean_centroid(&data, member_indices);

        let empty_kw: Vec<TopicKeyword> = Vec::new();
        let kw = keyword_sets.get(topic_idx).unwrap_or(&empty_kw);
        let keywords: Vec<String> = kw.iter().map(|k| k.word.clone()).collect();
        let keyword_scores: Vec<f64> = kw.iter().map(|k| k.score).collect();
        let label = if keywords.is_empty() {
            format!("topic_{topic_idx}")
        } else {
            keywords.join(" / ")
        };
        let memberships = vec![1.0f64; chunk_ids.len()];

        topics.push(TopicResult {
            cluster_index: topic_idx as i32,
            label,
            keywords,
            keyword_scores,
            chunk_ids,
            memberships,
            file_ids,
            project_names,
            avg_internal_similarity: avg_sim,
            representative_chunk_id,
            representative_snippet,
            top_files,
            centroid,
            parent_topic_ids: Vec::new(),
        });
    }

    // Metrics: label/coherence from topics + the free modularity signal.
    // Hard partition → each chunk fully belongs to its one topic.
    let mut metrics = TopicMetrics::from_topics(k, &topics);
    metrics.fill_coherence(&contents, &topics, DEFAULT_COHERENCE_TOP_N);
    metrics.modularity = louvain.modularity;
    metrics.mean_max_membership = 1.0;
    metrics.n_scored = n;

    ClusteringSummary {
        scope: scope.to_string(),
        chunks_analyzed: n,
        topics_found: topics.len(),
        noise_chunks,
        num_clusters: k,
        fuzziness: 0.0,
        converged: true,
        iterations: 1,
        topics,
        metrics: Some(metrics),
    }
}

fn empty_summary(scope: &str) -> ClusteringSummary {
    ClusteringSummary {
        scope: scope.to_string(),
        chunks_analyzed: 0,
        topics_found: 0,
        noise_chunks: 0,
        num_clusters: 0,
        fuzziness: 0.0,
        converged: true,
        iterations: 0,
        topics: Vec::new(),
        metrics: None,
    }
}

/// Mean pairwise cosine over member rows of L2-normalized `data` (sampled for
/// large communities). Mirrors `topic_clustering::similarity::avg_internal_similarity`.
fn cohesion(data: &Array2<f32>, indices: &[usize]) -> f64 {
    let n = indices.len();
    if n < 2 {
        return 1.0;
    }
    if n <= 100 {
        let mut sum = 0.0f64;
        let mut count = 0u64;
        for i in 0..n {
            for j in (i + 1)..n {
                sum += data.row(indices[i]).dot(&data.row(indices[j])) as f64;
                count += 1;
            }
        }
        if count > 0 { sum / count as f64 } else { 0.0 }
    } else {
        // Deterministic strided sampling (avoids an RNG dependency here).
        let step = (n / 100).max(1);
        let mut sum = 0.0f64;
        let mut count = 0u64;
        let mut i = 0;
        while i < n {
            let j = (i + step) % n;
            if i != j {
                sum += data.row(indices[i]).dot(&data.row(indices[j])) as f64;
                count += 1;
            }
            i += step;
        }
        if count > 0 { sum / count as f64 } else { 0.0 }
    }
}

/// Chunk id closest to the member mean.
fn representative(data: &Array2<f32>, chunk_ids: &[i64], member_indices: &[usize]) -> i64 {
    if chunk_ids.is_empty() {
        return 0;
    }
    if chunk_ids.len() == 1 {
        return chunk_ids[0];
    }
    let centroid = mean_centroid(data, member_indices);
    let mut best = 0usize;
    let mut best_sim = f32::NEG_INFINITY;
    for (local, &ri) in member_indices.iter().enumerate() {
        let row = data.row(ri);
        let mut sim = 0.0f32;
        for (a, b) in row.iter().zip(centroid.iter()) {
            sim += a * b;
        }
        if sim > best_sim {
            best_sim = sim;
            best = local;
        }
    }
    chunk_ids[best]
}

/// L2-normalized mean of member embeddings (1024-d).
fn mean_centroid(data: &Array2<f32>, member_indices: &[usize]) -> Vec<f32> {
    let dims = data.ncols();
    let mut c = vec![0.0f32; dims];
    for &i in member_indices {
        let row = data.row(i);
        for (acc, &v) in c.iter_mut().zip(row.iter()) {
            *acc += v;
        }
    }
    let m = member_indices.len().max(1) as f32;
    for v in &mut c {
        *v /= m;
    }
    let norm: f32 = c.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-12 {
        for v in &mut c {
            *v /= norm;
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(chunk_id: i64, file_id: i64, content: &str, emb: Vec<f32>) -> ChunkEmbeddingRow {
        ChunkEmbeddingRow {
            chunk_id,
            file_id,
            project_id: 1,
            project_name: "p".into(),
            path: format!("f{file_id}.rs"),
            language: "rust".into(),
            content: content.into(),
            embedding: emb,
        }
    }

    #[test]
    fn two_cliques_become_two_topics() {
        // 6 files, 2 cliques {1,2,3} and {4,5,6}; one chunk per file.
        let mut rows = Vec::new();
        for fid in 1..=6i64 {
            let mut e = vec![0.0f32; 8];
            e[(fid % 8) as usize] = 1.0;
            let word = if fid <= 3 {
                "alpha beta"
            } else {
                "gamma delta"
            };
            rows.push(row(fid, fid, word, e));
        }
        let mut edges = Vec::new();
        let clique = |a: i64, b: i64| GraphEdgeLite {
            src_file: a,
            dst_file: b,
            edge_type: EdgeType::Import,
            weight: 1.0,
        };
        // dense within each clique, none across.
        for &(a, b) in &[(1, 2), (2, 3), (1, 3), (4, 5), (5, 6), (4, 6)] {
            edges.push(clique(a, b));
        }
        let summary = cluster_graph(&rows, &edges, [1.0, 1.0, 1.0], 1.0, 1, 5, "test");
        assert_eq!(summary.num_clusters, 2, "expected 2 communities");
        assert_eq!(summary.topics_found, 2);
        // modularity for two well-separated cliques is strongly positive.
        let m = summary.metrics.expect("metrics");
        assert!(m.modularity > 0.3, "modularity={}", m.modularity);
    }

    #[test]
    fn empty_rows_empty_summary() {
        let summary = cluster_graph(&[], &[], [1.0, 1.0, 1.0], 1.0, 1, 5, "test");
        assert_eq!(summary.topics_found, 0);
        assert_eq!(summary.chunks_analyzed, 0);
    }

    #[test]
    fn edgeless_files_become_noise() {
        // 3 isolated files, no edges, min_cluster_size=2 → all singletons → noise.
        let rows: Vec<ChunkEmbeddingRow> = (1..=3i64)
            .map(|fid| {
                let mut e = vec![0.0f32; 4];
                e[(fid % 4) as usize] = 1.0;
                row(fid, fid, "x y z", e)
            })
            .collect();
        let summary = cluster_graph(&rows, &[], [1.0, 1.0, 1.0], 1.0, 2, 5, "test");
        assert_eq!(summary.topics_found, 0);
        assert_eq!(summary.noise_chunks, 3);
    }
}
