//! `topic_cooccurrence` collector — a topic-topic graph whose edges weight how
//! often a project's chunks are co-assigned to both topics. Louvain over it
//! yields topic communities; "bridge" topics (high participation across
//! communities) are the cross-cutting concerns.

use std::collections::HashMap;

use serde::Serialize;
use sqlx::PgPool;

use petgraph::graph::{DiGraph, NodeIndex};

use super::loaders::{TopicCoEdge, load_project_topic_histogram, load_topic_cooccurrence_edges};
use super::render::{Body, Renderable, Section, View};
use crate::graph::algorithms::louvain_communities;
use crate::graph::types::{EdgeType, EdgeWeight};

#[derive(Debug, Clone, Serialize)]
pub struct TopicCommunity {
    pub id: usize,
    pub topics: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BridgeTopic {
    pub topic_id: i32,
    pub label: String,
    /// Participation coefficient `1 − Σ_c (w_c/w_total)²` ∈ [0,1): 0 = all edges
    /// in one community, →1 = evenly spread across communities.
    pub participation: f64,
    pub communities_spanned: usize,
    pub inter_community_weight: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CooccurrenceReport {
    pub project: String,
    pub n_topics: usize,
    pub n_edges: usize,
    pub modularity: f64,
    pub communities: Vec<TopicCommunity>,
    pub bridge_topics: Vec<BridgeTopic>,
}

/// Compute bridge topics from the edge list + a topic→community assignment. Pure
/// (no graph/DB), so it is unit-testable independent of Louvain.
fn compute_bridges(
    edges: &[TopicCoEdge],
    node_community: &HashMap<i32, usize>,
    labels: &HashMap<i32, String>,
) -> Vec<BridgeTopic> {
    // Per-topic, sum incident edge weight bucketed by the neighbor's community.
    let mut per_topic: HashMap<i32, HashMap<usize, f64>> = HashMap::new();
    let mut add = |topic: i32, neighbor: i32, w: f64| {
        if let Some(&comm) = node_community.get(&neighbor) {
            *per_topic
                .entry(topic)
                .or_default()
                .entry(comm)
                .or_insert(0.0) += w;
        }
    };
    for e in edges {
        let w = e.co_count as f64;
        add(e.topic_a, e.topic_b, w);
        add(e.topic_b, e.topic_a, w);
    }

    let mut bridges = Vec::new();
    for (topic, buckets) in &per_topic {
        let total: f64 = buckets.values().sum();
        if total <= 0.0 || buckets.len() < 2 {
            continue; // not a bridge — all incident weight in one community
        }
        let own = node_community.get(topic).copied();
        let participation = 1.0 - buckets.values().map(|&w| (w / total).powi(2)).sum::<f64>();
        let inter: f64 = buckets
            .iter()
            .filter(|(c, _)| Some(**c) != own)
            .map(|(_, &w)| w)
            .sum();
        bridges.push(BridgeTopic {
            topic_id: *topic,
            label: labels
                .get(topic)
                .cloned()
                .unwrap_or_else(|| topic.to_string()),
            participation,
            communities_spanned: buckets.len(),
            inter_community_weight: inter,
        });
    }
    bridges.sort_by(|a, b| {
        b.inter_community_weight
            .partial_cmp(&a.inter_community_weight)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    bridges
}

/// Build the topic graph, run Louvain, and assemble the report. Pure given the
/// edges + labels.
fn analyze_cooccurrence(
    project: &str,
    edges: &[TopicCoEdge],
    labels: &HashMap<i32, String>,
) -> CooccurrenceReport {
    // Distinct topics → node indices.
    let mut idx_of: HashMap<i32, NodeIndex> = HashMap::new();
    let mut topic_of: HashMap<NodeIndex, i32> = HashMap::new();
    let mut graph: DiGraph<i32, EdgeWeight> = DiGraph::new();
    let mut ensure = |g: &mut DiGraph<i32, EdgeWeight>, t: i32| -> NodeIndex {
        if let Some(&ni) = idx_of.get(&t) {
            ni
        } else {
            let ni = g.add_node(t);
            idx_of.insert(t, ni);
            topic_of.insert(ni, t);
            ni
        }
    };
    for e in edges {
        let a = ensure(&mut graph, e.topic_a);
        let b = ensure(&mut graph, e.topic_b);
        // Symmetric co-occurrence → both directions (EdgeWeight isn't Copy).
        let mk = || EdgeWeight {
            edge_type: EdgeType::Semantic,
            weight: e.co_count as f64,
        };
        graph.add_edge(a, b, mk());
        graph.add_edge(b, a, mk());
    }

    let louvain = louvain_communities(&graph, 1.0);
    // topic_id → community id
    let node_community: HashMap<i32, usize> = louvain
        .communities
        .iter()
        .filter_map(|(&ni, &c)| topic_of.get(&ni).map(|&t| (t, c)))
        .collect();

    // Group topics by community.
    let mut by_comm: HashMap<usize, Vec<String>> = HashMap::new();
    for (&topic, &comm) in &node_community {
        by_comm.entry(comm).or_default().push(
            labels
                .get(&topic)
                .cloned()
                .unwrap_or_else(|| topic.to_string()),
        );
    }
    let mut communities: Vec<TopicCommunity> = by_comm
        .into_iter()
        .map(|(id, mut topics)| {
            topics.sort();
            TopicCommunity { id, topics }
        })
        .collect();
    communities.sort_by_key(|c| std::cmp::Reverse(c.topics.len()));

    let bridge_topics = compute_bridges(edges, &node_community, labels);

    CooccurrenceReport {
        project: project.to_string(),
        n_topics: idx_of.len(),
        n_edges: edges.len(),
        modularity: louvain.modularity,
        communities,
        bridge_topics,
    }
}

/// Collect the topic co-occurrence analysis for a project.
pub async fn collect_cooccurrence(
    pool: &PgPool,
    project_id: i32,
    project_name: &str,
    min_weight: i64,
) -> Result<CooccurrenceReport, sqlx::Error> {
    let edges = load_topic_cooccurrence_edges(pool, project_id, min_weight).await?;
    let hist = load_project_topic_histogram(pool, project_id).await?;
    let labels: HashMap<i32, String> = hist.into_iter().map(|r| (r.topic_id, r.label)).collect();
    Ok(analyze_cooccurrence(project_name, &edges, &labels))
}

fn fmt_f(x: f64) -> String {
    format!("{x:.3}")
}

impl Renderable for CooccurrenceReport {
    fn to_view(&self) -> View {
        let comm_rows: Vec<Vec<String>> = self
            .communities
            .iter()
            .map(|c| {
                vec![
                    c.id.to_string(),
                    c.topics.len().to_string(),
                    c.topics
                        .iter()
                        .take(12)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", "),
                ]
            })
            .collect();
        let bridge_rows: Vec<Vec<String>> = self
            .bridge_topics
            .iter()
            .take(20)
            .map(|b| {
                vec![
                    b.label.clone(),
                    fmt_f(b.participation),
                    b.communities_spanned.to_string(),
                    fmt_f(b.inter_community_weight),
                ]
            })
            .collect();
        View {
            title: format!("Topic concern coupling — {}", self.project),
            summary: vec![
                ("topics".into(), self.n_topics.to_string()),
                ("co_edges".into(), self.n_edges.to_string()),
                ("communities".into(), self.communities.len().to_string()),
                ("modularity".into(), fmt_f(self.modularity)),
            ],
            sections: vec![
                Section {
                    heading: "Topic communities (entangled concern groups)".into(),
                    body: if comm_rows.is_empty() {
                        Body::Note(
                            "No co-assigned chunks — topics are cleanly separated (or \
                             memberships are hard 1-per-chunk)."
                                .into(),
                        )
                    } else {
                        Body::Table {
                            headers: vec!["community".into(), "size".into(), "topics".into()],
                            rows: comm_rows,
                        }
                    },
                },
                Section {
                    heading: "Bridge topics (cross-cutting concerns)".into(),
                    body: if bridge_rows.is_empty() {
                        Body::Note("None — no topic bridges multiple communities.".into())
                    } else {
                        Body::Table {
                            headers: vec![
                                "topic".into(),
                                "participation".into(),
                                "communities".into(),
                                "inter_weight".into(),
                            ],
                            rows: bridge_rows,
                        }
                    },
                },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(a: i32, b: i32, w: i64) -> TopicCoEdge {
        TopicCoEdge {
            topic_a: a,
            topic_b: b,
            co_count: w,
        }
    }

    #[test]
    fn compute_bridges_flags_cross_community_topic() {
        // Communities: {1,2} and {3,4}. Topic 9 bridges both.
        let edges = vec![edge(1, 2, 5), edge(3, 4, 5), edge(1, 9, 2), edge(3, 9, 2)];
        let mut comm = HashMap::new();
        comm.insert(1, 0);
        comm.insert(2, 0);
        comm.insert(3, 1);
        comm.insert(4, 1);
        comm.insert(9, 0);
        let labels: HashMap<i32, String> = [(9, "bridge".to_string())].into_iter().collect();
        let bridges = compute_bridges(&edges, &comm, &labels);
        // Topic 9 has neighbors in both communities → a bridge.
        let b9 = bridges
            .iter()
            .find(|b| b.topic_id == 9)
            .expect("topic 9 bridges");
        assert_eq!(b9.communities_spanned, 2);
        assert!(b9.participation > 0.0);
        assert_eq!(b9.label, "bridge");
        // Topic 2 only touches its own community → not a bridge.
        assert!(bridges.iter().all(|b| b.topic_id != 2));
    }

    #[test]
    fn analyze_handles_empty_and_two_clusters() {
        let labels: HashMap<i32, String> = HashMap::new();
        let empty = analyze_cooccurrence("p", &[], &labels);
        assert_eq!(empty.n_topics, 0);
        assert_eq!(empty.n_edges, 0);
        assert!(empty.communities.is_empty());

        // Two dense triangles, weakly bridged → ≥2 communities.
        let edges = vec![
            edge(1, 2, 10),
            edge(2, 3, 10),
            edge(1, 3, 10),
            edge(4, 5, 10),
            edge(5, 6, 10),
            edge(4, 6, 10),
            edge(3, 4, 1),
        ];
        let r = analyze_cooccurrence("p", &edges, &labels);
        assert_eq!(r.n_topics, 6);
        assert_eq!(r.n_edges, 7);
        assert!(r.modularity.is_finite());
        assert!(
            r.communities.len() >= 2,
            "two weakly-linked triangles should split: {:?}",
            r.communities
        );
    }
}
