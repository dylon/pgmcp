//! `project_topic_similarity` collector — cluster projects by topic similarity
//! and flag redundant forks/backups. Two comparison spaces:
//! - **centroid** (default): each project = the L2-normalized mean of its chunk
//!   embeddings (shared BGE-M3 space), compared pairwise by cosine.
//! - **global_jsd**: each project = a distribution over the global roll-up
//!   topics (each chunk assigned to its nearest global centroid), compared by
//!   Jensen–Shannon distance.
//!
//! Clustering is agglomerative average-linkage at a similarity threshold.

use serde::Serialize;
use sqlx::PgPool;

use super::loaders::{load_project_chunk_embeddings, load_scope_centroids};
use super::measures;
use super::render::{Body, Renderable, Section, View};

/// Deterministic cap on chunk embeddings loaded per project (bounds cost on
/// large projects; the mean/assignment is stable for the first-N-by-id sample).
const MAX_CHUNKS_PER_PROJECT: i64 = 4000;

#[derive(Debug, Clone, Serialize)]
pub struct PairSim {
    pub a: String,
    pub b: String,
    pub sim: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Cluster {
    pub members: Vec<String>,
    pub avg_intra_sim: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ForkGroup {
    pub members: Vec<String>,
    pub avg_intra_sim: f64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SimilarityReport {
    pub method: String,
    pub n_projects: usize,
    pub threshold: f64,
    pub clusters: Vec<Cluster>,
    pub redundant_forks: Vec<ForkGroup>,
    pub pairwise_top: Vec<PairSim>,
}

/// Build per-project vectors (centroid method): the L2-normalized mean of each
/// project's chunk embeddings — a scope-agnostic point in the shared BGE-M3
/// space, comparable across projects.
async fn project_centroid_vectors(
    pool: &PgPool,
    projects: &[(i32, String)],
) -> Result<Vec<(String, Vec<f32>)>, sqlx::Error> {
    let mut out = Vec::new();
    for (pid, name) in projects {
        let embs = load_project_chunk_embeddings(pool, *pid, MAX_CHUNKS_PER_PROJECT).await?;
        if embs.is_empty() {
            continue;
        }
        let dim = embs[0].len();
        if dim == 0 {
            continue;
        }
        let mut acc = vec![0.0f32; dim];
        for e in &embs {
            if e.len() != dim {
                continue;
            }
            for i in 0..dim {
                acc[i] += e[i];
            }
        }
        measures::l2_normalize(&mut acc); // direction of the mean (count cancels)
        out.push((name.clone(), acc));
    }
    Ok(out)
}

/// Build per-project distributions over the global roll-up themes (global_jsd):
/// each of the project's chunks is assigned to its nearest global-theme centroid.
async fn project_global_distributions(
    pool: &PgPool,
    projects: &[(i32, String)],
) -> Result<Vec<(String, Vec<f64>)>, sqlx::Error> {
    let global = load_scope_centroids(pool, "global").await?;
    if global.is_empty() {
        return Ok(vec![]);
    }
    let g = global.len();
    let mut out = Vec::new();
    for (pid, name) in projects {
        let embs = load_project_chunk_embeddings(pool, *pid, MAX_CHUNKS_PER_PROJECT).await?;
        if embs.is_empty() {
            continue;
        }
        let mut dist = vec![0.0f64; g];
        for e in &embs {
            let mut best = 0usize;
            let mut best_sim = f32::NEG_INFINITY;
            for (gi, gc) in global.iter().enumerate() {
                let s = measures::cosine(e, &gc.centroid);
                if s > best_sim {
                    best_sim = s;
                    best = gi;
                }
            }
            dist[best] += 1.0;
        }
        out.push((name.clone(), dist));
    }
    Ok(out)
}

/// Longest common prefix of `names`, trimmed at the last separator and returned
/// only when ≥4 chars — the "family stem" used to recognize fork groups.
fn family_stem(names: &[String]) -> Option<String> {
    if names.len() < 2 {
        return None;
    }
    let first = names[0].as_bytes();
    let mut len = first.len();
    for n in &names[1..] {
        let nb = n.as_bytes();
        let mut i = 0;
        while i < len && i < nb.len() && first[i] == nb[i] {
            i += 1;
        }
        len = i;
    }
    let mut stem = String::from_utf8_lossy(&first[..len]).to_string();
    while let Some(c) = stem.chars().last() {
        if c == '-' || c == '_' || c == '.' {
            stem.pop();
        } else {
            break;
        }
    }
    if stem.len() >= 4 { Some(stem) } else { None }
}

/// Agglomerative average-linkage clustering over a symmetric similarity matrix.
/// Returns clusters (member-index lists) where every merge had average
/// cross-similarity ≥ `threshold`.
fn average_linkage(sim: &[Vec<f32>], threshold: f32) -> Vec<Vec<usize>> {
    let n = sim.len();
    let mut clusters: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();
    loop {
        let mut best = (f32::NEG_INFINITY, 0usize, 0usize);
        for i in 0..clusters.len() {
            for j in (i + 1)..clusters.len() {
                let mut total = 0.0f32;
                let mut cnt = 0u32;
                for &a in &clusters[i] {
                    for &b in &clusters[j] {
                        total += sim[a][b];
                        cnt += 1;
                    }
                }
                let avg = if cnt == 0 { 0.0 } else { total / cnt as f32 };
                if avg > best.0 {
                    best = (avg, i, j);
                }
            }
        }
        if best.0 < threshold || clusters.len() < 2 {
            break;
        }
        let (_, i, j) = best;
        let merged: Vec<usize> = clusters[i]
            .iter()
            .chain(clusters[j].iter())
            .copied()
            .collect();
        // Remove j first (higher index) then i to keep indices valid.
        clusters.remove(j);
        clusters.remove(i);
        clusters.push(merged);
    }
    clusters
}

fn avg_intra(members: &[usize], sim: &[Vec<f32>]) -> f64 {
    if members.len() < 2 {
        return 1.0;
    }
    let mut total = 0.0f64;
    let mut cnt = 0u32;
    for i in 0..members.len() {
        for j in (i + 1)..members.len() {
            total += sim[members[i]][members[j]] as f64;
            cnt += 1;
        }
    }
    if cnt == 0 { 0.0 } else { total / cnt as f64 }
}

/// Assemble the report from `(name, sim_matrix)`.
fn assemble(
    method: &str,
    names: Vec<String>,
    sim: Vec<Vec<f32>>,
    threshold: f64,
) -> SimilarityReport {
    let n = names.len();
    let clusters_idx = average_linkage(&sim, threshold as f32);

    let mut clusters = Vec::new();
    let mut redundant_forks = Vec::new();
    for c in &clusters_idx {
        if c.len() < 2 {
            continue;
        }
        let members: Vec<String> = c.iter().map(|&i| names[i].clone()).collect();
        let intra = avg_intra(c, &sim);
        clusters.push(Cluster {
            members: members.clone(),
            avg_intra_sim: intra,
        });
        let stem = family_stem(&members);
        let has_bak = members.iter().any(|m| m.contains(".bak"));
        if stem.is_some() || has_bak {
            let mut reasons = Vec::new();
            if let Some(s) = &stem {
                reasons.push(format!("shared name stem '{s}'"));
            }
            if has_bak {
                reasons.push("contains a .bak backup".into());
            }
            redundant_forks.push(ForkGroup {
                members,
                avg_intra_sim: intra,
                reason: reasons.join("; "),
            });
        }
    }
    clusters.sort_by(|a, b| {
        b.avg_intra_sim
            .partial_cmp(&a.avg_intra_sim)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    redundant_forks.sort_by(|a, b| {
        b.avg_intra_sim
            .partial_cmp(&a.avg_intra_sim)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut pairs = Vec::new();
    for i in 0..n {
        for j in (i + 1)..n {
            pairs.push(PairSim {
                a: names[i].clone(),
                b: names[j].clone(),
                sim: sim[i][j] as f64,
            });
        }
    }
    pairs.sort_by(|a, b| {
        b.sim
            .partial_cmp(&a.sim)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    pairs.truncate(15);

    SimilarityReport {
        method: method.to_string(),
        n_projects: n,
        threshold,
        clusters,
        redundant_forks,
        pairwise_top: pairs,
    }
}

/// Run the similarity analysis over `projects` (`(id, name)` pairs) using
/// `method` and `threshold`.
pub async fn collect_similarity(
    pool: &PgPool,
    projects: &[(i32, String)],
    method: &str,
    threshold: f64,
) -> Result<SimilarityReport, sqlx::Error> {
    let (names, sim): (Vec<String>, Vec<Vec<f32>>) = if method == "global_jsd" {
        let dists = project_global_distributions(pool, projects).await?;
        let names: Vec<String> = dists.iter().map(|(n, _)| n.clone()).collect();
        let n = names.len();
        let mut sim = vec![vec![0.0f32; n]; n];
        for i in 0..n {
            for j in 0..n {
                sim[i][j] = if i == j {
                    1.0
                } else {
                    let jsd = measures::js_divergence(&dists[i].1, &dists[j].1);
                    1.0 - jsd.sqrt() as f32
                };
            }
        }
        (names, sim)
    } else {
        let vecs = project_centroid_vectors(pool, projects).await?;
        let names: Vec<String> = vecs.iter().map(|(n, _)| n.clone()).collect();
        let n = names.len();
        let mut sim = vec![vec![0.0f32; n]; n];
        for i in 0..n {
            for j in 0..n {
                sim[i][j] = if i == j {
                    1.0
                } else {
                    measures::cosine(&vecs[i].1, &vecs[j].1)
                };
            }
        }
        (names, sim)
    };
    Ok(assemble(method, names, sim, threshold))
}

fn fmt_f(x: f64) -> String {
    format!("{x:.3}")
}

impl Renderable for SimilarityReport {
    fn to_view(&self) -> View {
        let fork_rows: Vec<Vec<String>> = self
            .redundant_forks
            .iter()
            .map(|f| {
                vec![
                    f.members.join(", "),
                    fmt_f(f.avg_intra_sim),
                    f.reason.clone(),
                ]
            })
            .collect();
        let cluster_rows: Vec<Vec<String>> = self
            .clusters
            .iter()
            .map(|c| vec![c.members.join(", "), fmt_f(c.avg_intra_sim)])
            .collect();
        let pair_rows: Vec<Vec<String>> = self
            .pairwise_top
            .iter()
            .map(|p| vec![p.a.clone(), p.b.clone(), fmt_f(p.sim)])
            .collect();
        View {
            title: "Project similarity by topics".into(),
            summary: vec![
                ("method".into(), self.method.clone()),
                ("projects".into(), self.n_projects.to_string()),
                ("threshold".into(), fmt_f(self.threshold)),
                (
                    "redundant_fork_groups".into(),
                    self.redundant_forks.len().to_string(),
                ),
            ],
            sections: vec![
                Section {
                    heading: "Likely redundant forks / backups".into(),
                    body: if fork_rows.is_empty() {
                        Body::Note("None detected at this threshold.".into())
                    } else {
                        Body::Table {
                            headers: vec!["members".into(), "avg_sim".into(), "reason".into()],
                            rows: fork_rows,
                        }
                    },
                },
                Section {
                    heading: "Similar project clusters".into(),
                    body: if cluster_rows.is_empty() {
                        Body::Note("No clusters above threshold.".into())
                    } else {
                        Body::Table {
                            headers: vec!["members".into(), "avg_sim".into()],
                            rows: cluster_rows,
                        }
                    },
                },
                Section {
                    heading: "Most similar project pairs".into(),
                    body: Body::Table {
                        headers: vec!["a".into(), "b".into(), "sim".into()],
                        rows: pair_rows,
                    },
                },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_stem_finds_fork_base() {
        let names = vec![
            "MeTTa-Compiler".to_string(),
            "MeTTa-Compiler-PR-63".to_string(),
            "MeTTa-Compiler.bak".to_string(),
        ];
        assert_eq!(family_stem(&names).as_deref(), Some("MeTTa-Compiler"));
        // Distinct projects share no meaningful stem.
        assert_eq!(family_stem(&["auth".into(), "database".into()]), None);
        // A single project is not a family.
        assert_eq!(family_stem(&["solo".into()]), None);
    }

    #[test]
    fn average_linkage_merges_only_above_threshold() {
        // 3 items: 0,1 very similar; 2 distant.
        let sim = vec![
            vec![1.0, 0.95, 0.10],
            vec![0.95, 1.0, 0.12],
            vec![0.10, 0.12, 1.0],
        ];
        let clusters = average_linkage(&sim, 0.85);
        // {0,1} merge; 2 stays alone.
        assert_eq!(clusters.len(), 2);
        assert!(clusters.iter().any(|c| c.len() == 2));
        assert!(clusters.iter().any(|c| c == &vec![2usize]));
        // A high threshold leaves everything separate.
        assert_eq!(average_linkage(&sim, 0.99).len(), 3);
    }

    #[test]
    fn assemble_flags_bak_fork_and_low_sim_pairs() {
        let names = vec![
            "proj".to_string(),
            "proj.bak".to_string(),
            "other".to_string(),
        ];
        let sim = vec![
            vec![1.0, 0.97, 0.05],
            vec![0.97, 1.0, 0.04],
            vec![0.05, 0.04, 1.0],
        ];
        let r = assemble("centroid", names, sim, 0.85);
        assert_eq!(r.n_projects, 3);
        // proj + proj.bak cluster and are flagged as a redundant fork.
        assert_eq!(r.redundant_forks.len(), 1);
        let fork = &r.redundant_forks[0];
        assert!(fork.members.contains(&"proj.bak".to_string()));
        assert!(fork.reason.contains(".bak") || fork.reason.contains("stem"));
        // 'other' is in no cluster.
        assert!(
            r.clusters
                .iter()
                .all(|c| !c.members.contains(&"other".to_string()))
        );
    }
}
