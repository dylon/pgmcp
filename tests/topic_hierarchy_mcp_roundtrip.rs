//! Regression test for Phase 9 (hierarchy meta-clustering).
//!
//! Constructs a synthetic set of 100 global topic centroids drawn from
//! 5 well-separated meta-clusters and asserts that
//! `cluster_topic_hierarchy` recovers a meta-group count within the
//! clamp range `[4, 50]` (per `estimate_k_meta(k_global) = clamp(sqrt(K), 4, 50)`)
//! and that every input topic gets assigned to exactly one meta-group.

use pgmcp::cron::topic_hierarchy::{
    TopicCentroid, cluster_topic_hierarchy, cluster_topic_hierarchy_seeded, estimate_k_meta,
};

/// Build 100 synthetic centroids in 5 well-separated meta-clusters.
/// Each meta-cluster has 20 topics; within a meta-cluster the topics
/// share a base direction with small per-topic jitter.
fn build_synthetic_centroids() -> Vec<TopicCentroid> {
    let d = 16usize;
    let n_meta = 5usize;
    let per_meta = 20usize;

    let mut centroids = Vec::with_capacity(n_meta * per_meta);

    let mut s: u64 = 0x000B_ADC0_FFEE_DEADu64;
    let mut next_f = || {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((s >> 33) as f32 / (1u32 << 31) as f32) - 0.5 // ~[-0.5, +0.5]
    };

    for m in 0..n_meta {
        // Base direction for this meta: one dominant dimension per meta.
        let mut base = vec![0.0f32; d];
        base[m * (d / n_meta)] = 1.0;

        for t in 0..per_meta {
            let mut centroid = base.clone();
            // Tiny within-meta jitter so the topics aren't identical.
            for slot in centroid.iter_mut() {
                *slot += 0.05 * next_f();
            }
            let _ = d; // length already encoded in centroid; keep d for caller context
            // L2-normalize.
            let norm: f32 = centroid.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 1e-12 {
                for v in &mut centroid {
                    *v /= norm;
                }
            }
            centroids.push(TopicCentroid {
                topic_id: (m * per_meta + t) as i64,
                label: format!("topic_{m}_{t}"),
                centroid,
            });
        }
    }

    centroids
}

#[test]
fn cluster_topic_hierarchy_returns_meta_groups_in_clamp_range() {
    let inputs = build_synthetic_centroids();
    let k_global = inputs.len();
    assert_eq!(k_global, 100);

    let k_meta_expected = estimate_k_meta(k_global);
    assert!(
        (4..=50).contains(&k_meta_expected),
        "estimate_k_meta(100) must be in [4, 50], got {k_meta_expected}"
    );

    // Seeded so the meta-group count is deterministic (FCM k-means++ init is
    // otherwise system-RNG seeded, which made this regression test flaky).
    let (meta_groups, fcm_result) = cluster_topic_hierarchy_seeded(&inputs, 2.0, 30, 1e-3, 42);

    // The number of non-empty meta-groups must be in the clamp range.
    assert!(
        (4..=50).contains(&meta_groups.len()),
        "meta_groups.len() must be in [4, 50], got {}",
        meta_groups.len()
    );

    // FCM must have converged (or at least taken non-zero iterations).
    assert!(
        fcm_result.iterations > 0,
        "hierarchy FCM took {} iterations, expected > 0",
        fcm_result.iterations
    );

    // Every input topic must be assigned to exactly one meta-group.
    let mut seen_ids = std::collections::HashSet::new();
    for group in &meta_groups {
        for &id in &group.parent_topic_ids {
            assert!(
                seen_ids.insert(id),
                "topic {id} assigned to multiple meta-groups"
            );
        }
    }
    assert_eq!(
        seen_ids.len(),
        k_global,
        "every input topic must be assigned to a meta-group"
    );
}

#[test]
fn estimate_k_meta_documented_examples() {
    // From the impl: clamp(sqrt(k_global), 4, 50), capped by k_global.
    assert_eq!(estimate_k_meta(1), 1, "tiny K stays clamped by k_global");
    assert_eq!(estimate_k_meta(4), 4);
    assert_eq!(estimate_k_meta(100), 10);
    assert_eq!(estimate_k_meta(10_000), 50, "huge K clamps at upper bound");
}

#[test]
fn cluster_topic_hierarchy_handles_empty_input() {
    let (meta_groups, fcm_result) = cluster_topic_hierarchy(&[], 2.0, 10, 1e-3);
    assert!(meta_groups.is_empty());
    assert_eq!(fcm_result.iterations, 0);
}
