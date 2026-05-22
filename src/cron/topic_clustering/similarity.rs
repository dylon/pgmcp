//! L2 normalization + cluster-similarity helpers — extracted from the
//! parent `topic_clustering.rs` as part of the D.2 god-file split.

use ndarray::ArrayView2;

// ============================================================================
// L2 normalization
// ============================================================================

/// L2-normalize a vector in-place. After normalization, Euclidean distance
/// is proportional to cosine distance: ||a-b||^2 = 2(1 - cos(a,b)).
pub(super) fn l2_normalize(v: &mut [f64]) {
    let norm: f64 = v.iter().map(|x| x * x).sum::<f64>().sqrt();
    if norm > 1e-12 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

// ============================================================================
// Similarity computation
// ============================================================================

/// Compute average cosine similarity for a cluster of L2-normalized embeddings.
/// For clusters <= 100 chunks: all-pairs. For larger: random sample of 100 pairs.
///
/// Operates on a view of the full data matrix + a list of member indices —
/// no per-member `Vec<f32>` duplication.
pub(super) fn avg_internal_similarity(data: &ArrayView2<f32>, indices: &[usize]) -> f64 {
    let n = indices.len();
    if n < 2 {
        return 1.0;
    }

    if n <= 100 {
        let mut sum: f64 = 0.0;
        let mut count = 0u64;
        for i in 0..n {
            for j in (i + 1)..n {
                sum += data.row(indices[i]).dot(&data.row(indices[j])) as f64;
                count += 1;
            }
        }
        if count > 0 { sum / count as f64 } else { 0.0 }
    } else {
        use rand::Rng;
        let mut rng = rand::rng();
        let mut sum: f64 = 0.0;
        let samples = 100u64;
        for _ in 0..samples {
            let i = rng.random_range(0..n);
            let j = rng.random_range(0..n);
            if i != j {
                sum += data.row(indices[i]).dot(&data.row(indices[j])) as f64;
            }
        }
        sum / samples as f64
    }
}

/// Find the chunk closest to the centroid of its members (representative chunk).
///
/// `chunk_ids` and `member_indices` are parallel: `member_indices[i]` is the
/// row in `data`, `chunk_ids[i]` is the chunk's DB id. Returns the DB id of
/// the member whose embedding is closest to the mean.
pub(super) fn find_representative(
    data: &ArrayView2<f32>,
    chunk_ids: &[i64],
    member_indices: &[usize],
) -> i64 {
    if chunk_ids.is_empty() {
        return 0;
    }
    if chunk_ids.len() == 1 {
        return chunk_ids[0];
    }

    let dims = data.ncols();
    let n = member_indices.len() as f32;

    let mut centroid = vec![0.0_f32; dims];
    for &idx in member_indices {
        let row = data.row(idx);
        for (c, &v) in centroid.iter_mut().zip(row.iter()) {
            *c += v;
        }
    }
    for v in &mut centroid {
        *v /= n;
    }

    let mut best_idx = 0;
    let mut best_sim = f32::NEG_INFINITY;
    for (local_i, &row_idx) in member_indices.iter().enumerate() {
        let row = data.row(row_idx);
        // Cosine on L2-normalized = dot(row, centroid). centroid here is
        // the member mean (not re-normalized); best_idx remains correct for
        // argmax regardless of the (constant) magnitude.
        let mut sim: f32 = 0.0;
        for (a, b) in row.iter().zip(centroid.iter()) {
            sim += a * b;
        }
        if sim > best_sim {
            best_sim = sim;
            best_idx = local_i;
        }
    }

    chunk_ids[best_idx]
}
