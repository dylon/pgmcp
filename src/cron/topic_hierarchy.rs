//! Phase 9 — meta-clustering hierarchy on global topic centroids.
//!
//! Given the `K` global topic centroids produced by Phase 1's FCM, cluster
//! their centroids into `K_meta < K` meta-groups. The resulting hierarchy is
//! stored in `code_topics` with `scope = "hierarchy"` and `parent_topic_ids`
//! pointing at the global topics composing each meta-group.
//!
//! This is a complementary view **on top of** the global chunk-to-topic
//! assignments; the global assignments themselves remain unchanged and
//! authoritative (cross-document comparability invariant).

use ndarray::Array2;
use tracing::info;

use crate::cron::topic_clustering::{FcmResult, fuzzy_c_means, fuzzy_c_means_seeded};

/// One input to hierarchy clustering: a global topic with its centroid.
#[derive(Debug, Clone)]
pub struct TopicCentroid {
    pub topic_id: i64,
    pub label: String,
    pub centroid: Vec<f32>,
}

/// One output meta-group.
#[derive(Debug, Clone)]
pub struct MetaGroup {
    pub cluster_index: usize,
    pub parent_topic_ids: Vec<i64>,
    pub parent_labels: Vec<String>,
}

/// Estimate K_meta for the hierarchy: `clamp(sqrt(K), 4, 50)`.
pub fn estimate_k_meta(k_global: usize) -> usize {
    let k = (k_global as f64).sqrt().round() as usize;
    k.clamp(4, 50).min(k_global)
}

/// Cluster global topic centroids into meta-groups via FCM.
///
/// Returns `(meta_groups, fcm_result)`. `meta_groups[i]` is the i-th
/// meta-group with its member global topic IDs. `fcm_result.centroids` are
/// the meta-centroids (K_meta × d).
///
/// Each global topic is assigned to the meta-group whose centroid it's most
/// fuzzy-member of (argmax over the final membership row).
pub fn cluster_topic_hierarchy(
    inputs: &[TopicCentroid],
    m: f64,
    max_iters: usize,
    tolerance: f64,
) -> (Vec<MetaGroup>, FcmResult) {
    cluster_topic_hierarchy_with_runner(
        inputs,
        m,
        max_iters,
        tolerance,
        |data, k, m, max_iters, tolerance| fuzzy_c_means(data, k, m, max_iters, tolerance, None),
    )
}

/// Deterministic variant of [`cluster_topic_hierarchy`] that seeds FCM's
/// k-means++ init for reproducible meta-grouping. Production uses the
/// non-seeded version (system RNG); regression tests use this to avoid flaky
/// meta-group counts from a bad random init.
pub fn cluster_topic_hierarchy_seeded(
    inputs: &[TopicCentroid],
    m: f64,
    max_iters: usize,
    tolerance: f64,
    seed: u64,
) -> (Vec<MetaGroup>, FcmResult) {
    cluster_topic_hierarchy_with_runner(
        inputs,
        m,
        max_iters,
        tolerance,
        move |data, k, m, max_iters, tolerance| {
            fuzzy_c_means_seeded(data, k, m, max_iters, tolerance, seed)
        },
    )
}

fn cluster_topic_hierarchy_with_runner<F>(
    inputs: &[TopicCentroid],
    m: f64,
    max_iters: usize,
    tolerance: f64,
    mut run_fcm: F,
) -> (Vec<MetaGroup>, FcmResult)
where
    F: for<'a> FnMut(ndarray::ArrayView2<'a, f32>, usize, f64, usize, f64) -> FcmResult,
{
    if inputs.is_empty() {
        return (
            Vec::new(),
            FcmResult {
                membership: Array2::<f32>::zeros((0, 0)),
                centroids: Array2::<f32>::zeros((0, 0)),
                iterations: 0,
                converged: false,
                cancelled: false,
                inertia: 0.0,
            },
        );
    }

    let k_global = inputs.len();
    let d = inputs[0].centroid.len();
    let k_meta = estimate_k_meta(k_global);

    info!(
        k_global,
        k_meta, d, "Running hierarchy FCM on global centroids"
    );

    // Build the data matrix from centroids. Assumes L2-normalised embeddings
    // (fastembed output already is); we do NOT re-normalise here to avoid
    // subtly distorting a caller that normalised differently.
    let mut data = Array2::<f32>::zeros((k_global, d));
    for (i, input) in inputs.iter().enumerate() {
        assert_eq!(
            input.centroid.len(),
            d,
            "centroid dim mismatch at topic {}",
            i
        );
        for (j, &v) in input.centroid.iter().enumerate() {
            data[[i, j]] = v;
        }
    }

    let fcm_result = run_fcm(data.view(), k_meta, m, max_iters, tolerance);

    // Assign each global topic to its argmax meta-cluster.
    let mut groups: Vec<Vec<(i64, String)>> = vec![Vec::new(); k_meta];
    for (i, input) in inputs.iter().enumerate() {
        let row = fcm_result.membership.row(i);
        let best_meta = row
            .iter()
            .enumerate()
            .fold((0, f32::NEG_INFINITY), |(best_idx, best_mu), (idx, &mu)| {
                if mu > best_mu {
                    (idx, mu)
                } else {
                    (best_idx, best_mu)
                }
            })
            .0;
        groups[best_meta].push((input.topic_id, input.label.clone()));
    }

    let meta_groups: Vec<MetaGroup> = groups
        .into_iter()
        .enumerate()
        .filter(|(_, members)| !members.is_empty())
        .map(|(idx, members)| {
            let (ids, labels): (Vec<i64>, Vec<String>) = members.into_iter().unzip();
            MetaGroup {
                cluster_index: idx,
                parent_topic_ids: ids,
                parent_labels: labels,
            }
        })
        .collect();

    info!(
        meta_groups = meta_groups.len(),
        converged = fcm_result.converged,
        "Hierarchy FCM complete"
    );

    (meta_groups, fcm_result)
}

/// Build a human-readable label for a meta-group by joining the top-N
/// parent-topic labels with " | ".
pub fn label_meta_group(group: &MetaGroup, top_n: usize) -> String {
    if group.parent_labels.is_empty() {
        return format!("meta_{}", group.cluster_index);
    }
    let n = top_n.min(group.parent_labels.len());
    let joined = group.parent_labels[..n].join(" | ");
    format!("meta_{}: {}", group.cluster_index, joined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::topic_clustering::fuzzy_c_means_seeded;

    fn make_centroid(topic_id: i64, label: &str, v0: f32, v1: f32) -> TopicCentroid {
        // 4-D centroid, L2-normalised.
        let raw = [v0, v1, 0.0, 0.0];
        let norm: f32 = raw.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
        TopicCentroid {
            topic_id,
            label: label.into(),
            centroid: raw.iter().map(|v| v / norm).collect(),
        }
    }

    #[test]
    fn test_estimate_k_meta_bounds() {
        assert_eq!(estimate_k_meta(1), 1); // min(4, k_global) = 1
        assert_eq!(estimate_k_meta(4), 4);
        assert_eq!(estimate_k_meta(100), 10);
        assert_eq!(estimate_k_meta(10000), 50); // upper clamp
    }

    #[test]
    fn test_hierarchy_clusters_semantically_related() {
        // 6 input topics forming 2 semantic groups in 4D.
        //   group A: axis 0 — topics 1, 2, 3 near (1, 0, 0, 0)
        //   group B: axis 1 — topics 4, 5, 6 near (0, 1, 0, 0)
        // estimate_k_meta(6) = clamp(sqrt(6)≈2, 4, 50) = 4, so FCM runs with
        // K=4 meta-clusters on 6 points. Empty meta-groups are filtered out.
        let inputs = vec![
            make_centroid(1, "db", 1.0, 0.01),
            make_centroid(2, "query", 0.99, 0.02),
            make_centroid(3, "index", 0.98, 0.0),
            make_centroid(4, "http", 0.0, 1.0),
            make_centroid(5, "router", 0.02, 0.99),
            make_centroid(6, "auth", 0.01, 0.98),
        ];

        let (groups, _result) = cluster_topic_hierarchy_with_runner(
            &inputs,
            2.0,
            100,
            1e-5,
            |data, k, m, max_iters, tolerance| {
                fuzzy_c_means_seeded(data, k, m, max_iters, tolerance, 42)
            },
        );
        // K_meta=4 on 6 points may leave 0-2 groups empty after argmax
        // assignment; the algorithm filters them. Require at least 2
        // (the natural groupings) and no more than 4.
        assert!(
            (2..=4).contains(&groups.len()),
            "expected 2-4 meta-groups, got {}",
            groups.len()
        );

        // No group should mix across the semantic boundary (topics 1..=3 and
        // topics 4..=6 must never appear together in one meta-group).
        for g in &groups {
            let has_small = g.parent_topic_ids.iter().any(|&id| id <= 3);
            let has_large = g.parent_topic_ids.iter().any(|&id| id >= 4);
            assert!(
                !(has_small && has_large),
                "group {} mixed across semantic boundaries: {:?}",
                g.cluster_index,
                g.parent_topic_ids
            );
        }

        // Every topic must be assigned to exactly one group (sum of group sizes = 6).
        let total: usize = groups.iter().map(|g| g.parent_topic_ids.len()).sum();
        assert_eq!(total, 6, "all 6 topics must be assigned, got {}", total);
    }

    #[test]
    fn test_hierarchy_empty_input() {
        let (groups, result) = cluster_topic_hierarchy(&[], 2.0, 10, 1e-5);
        assert!(groups.is_empty());
        assert_eq!(result.iterations, 0);
    }

    #[test]
    fn test_label_meta_group() {
        let group = MetaGroup {
            cluster_index: 3,
            parent_topic_ids: vec![1, 2, 3],
            parent_labels: vec!["db".into(), "query".into(), "index".into()],
        };
        let label = label_meta_group(&group, 2);
        assert_eq!(label, "meta_3: db | query");
    }

    #[test]
    fn test_label_empty_meta_group() {
        let group = MetaGroup {
            cluster_index: 7,
            parent_topic_ids: Vec::new(),
            parent_labels: Vec::new(),
        };
        assert_eq!(label_meta_group(&group, 3), "meta_7");
    }
}
