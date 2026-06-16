//! Pure information-theoretic / geometric measures over topic distributions —
//! no DB, no [`SystemContext`](crate::context::SystemContext), so trivially
//! unit-testable and shared across the `topic_analysis` collectors (mirrors the
//! design of [`crate::quality::forecast`]).
//!
//! - **Specialization** of a project = how concentrated its chunks are across
//!   its topics: Shannon entropy (normalized) and the Gini coefficient.
//! - **Ownership concentration** of a topic = Herfindahl over author shares.
//! - **Project similarity** = cosine over aggregated topic centroids (the shared
//!   BGE-M3 embedding space) or Jensen–Shannon over global-topic distributions.

/// Magnitudes below this are treated as zero (avoids dividing by a hair).
const EPS: f64 = 1e-12;

/// Shannon entropy `H = -Σ pᵢ ln pᵢ` (nats) of a non-negative count vector,
/// where `pᵢ = cᵢ / Σc`. Empty input or a single non-zero bucket → `0.0`
/// (perfectly concentrated). Zero counts are skipped (`0·ln0 ≡ 0`).
pub fn shannon_entropy(counts: &[f64]) -> f64 {
    let total: f64 = counts.iter().filter(|&&c| c > 0.0).sum();
    if total <= EPS {
        return 0.0;
    }
    let mut h = 0.0;
    for &c in counts {
        if c > 0.0 {
            let p = c / total;
            h -= p * p.ln();
        }
    }
    h
}

/// Shannon entropy normalized to `[0, 1]` by dividing by `ln K`, where `K` is
/// the number of non-empty buckets. `0` = a single dominant topic, `1` = a
/// perfectly uniform spread. `K < 2` → `0.0` (no spread is possible).
pub fn normalized_entropy(counts: &[f64]) -> f64 {
    let k = counts.iter().filter(|&&c| c > 0.0).count();
    if k < 2 {
        return 0.0;
    }
    shannon_entropy(counts) / (k as f64).ln()
}

/// Specialization index `= 1 − normalized_entropy` ∈ `[0, 1]`. `1` = a focused
/// single-theme project, `0` = a generalist project spread evenly across themes.
pub fn specialization_index(counts: &[f64]) -> f64 {
    1.0 - normalized_entropy(counts)
}

/// Gini coefficient ∈ `[0, 1)` of a non-negative count vector: `0` = perfectly
/// even, `→1` = all mass in one bucket. Computed `O(n log n)` on the sorted
/// counts via `G = (2·Σ i·x₍ᵢ₎)/(n·Σx) − (n+1)/n` (i 1-based, ascending).
/// `n < 2` or zero total → `0.0`.
pub fn gini(counts: &[f64]) -> f64 {
    let n = counts.len();
    if n < 2 {
        return 0.0;
    }
    let total: f64 = counts.iter().sum();
    if total <= EPS {
        return 0.0;
    }
    let mut sorted: Vec<f64> = counts.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut weighted = 0.0;
    for (i, &x) in sorted.iter().enumerate() {
        weighted += ((i + 1) as f64) * x;
    }
    let n_f = n as f64;
    let g = (2.0 * weighted) / (n_f * total) - (n_f + 1.0) / n_f;
    g.clamp(0.0, 1.0)
}

/// Herfindahl–Hirschman index `Σ sᵢ²` over shares `sᵢ = cᵢ / Σc` ∈ `[0, 1]`.
/// `1/K` for `K` even buckets, `1` for a monopoly. Used for topic-ownership
/// concentration. Zero total → `0.0`.
pub fn herfindahl(counts: &[f64]) -> f64 {
    let total: f64 = counts.iter().sum();
    if total <= EPS {
        return 0.0;
    }
    counts
        .iter()
        .map(|&c| {
            let s = c / total;
            s * s
        })
        .sum()
}

/// Bus factor: the minimum number of top contributors whose shares sum to at
/// least `coverage` (e.g. `0.5` = "who owns half the lines"). `1` = a single
/// person owns ≥`coverage`. Empty / zero total → `0`.
pub fn bus_factor(counts: &[f64], coverage: f64) -> usize {
    let total: f64 = counts.iter().sum();
    if total <= EPS {
        return 0;
    }
    let mut sorted: Vec<f64> = counts.iter().copied().filter(|&c| c > 0.0).collect();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let target = coverage.clamp(0.0, 1.0) * total;
    let mut acc = 0.0;
    for (i, &c) in sorted.iter().enumerate() {
        acc += c;
        if acc >= target - EPS {
            return i + 1;
        }
    }
    sorted.len()
}

/// In-place L2 normalization of an `f32` vector. A zero vector is left as-is.
pub fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-12 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Cosine similarity of two equal-length `f32` vectors ∈ `[−1, 1]`. Mismatched
/// lengths or a zero vector → `0.0`.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom < 1e-12 { 0.0 } else { dot / denom }
}

/// Jensen–Shannon divergence (base-2, so the result is in `[0, 1]`) between two
/// distributions given as non-negative weight vectors over the SAME support
/// (they are normalized internally). `0` = identical, `1` = disjoint support.
/// The square root of this is the Jensen–Shannon *distance* (a true metric),
/// which callers turn into a similarity `1 − √JSD`.
pub fn js_divergence(p: &[f64], q: &[f64]) -> f64 {
    if p.len() != q.len() || p.is_empty() {
        return 1.0;
    }
    let sp: f64 = p.iter().sum();
    let sq: f64 = q.iter().sum();
    if sp <= EPS || sq <= EPS {
        return 1.0;
    }
    let ln2 = std::f64::consts::LN_2;
    let mut jsd = 0.0;
    for i in 0..p.len() {
        let pi = p[i] / sp;
        let qi = q[i] / sq;
        let mi = 0.5 * (pi + qi);
        if pi > 0.0 {
            jsd += 0.5 * pi * (pi / mi).ln() / ln2;
        }
        if qi > 0.0 {
            jsd += 0.5 * qi * (qi / mi).ln() / ln2;
        }
    }
    jsd.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn entropy_uniform_is_max_specialization_is_zero() {
        let uniform = [1.0, 1.0, 1.0, 1.0];
        assert!(approx(shannon_entropy(&uniform), 4.0_f64.ln()));
        assert!(approx(normalized_entropy(&uniform), 1.0));
        assert!(approx(specialization_index(&uniform), 0.0));
    }

    #[test]
    fn entropy_single_theme_is_full_specialization() {
        assert!(approx(shannon_entropy(&[10.0, 0.0, 0.0]), 0.0));
        assert!(approx(normalized_entropy(&[10.0, 0.0, 0.0]), 0.0));
        assert!(approx(specialization_index(&[10.0, 0.0, 0.0]), 1.0));
        // A single bucket has no spread.
        assert!(approx(specialization_index(&[7.0]), 1.0));
        // Empty.
        assert!(approx(shannon_entropy(&[]), 0.0));
    }

    #[test]
    fn gini_even_is_zero_concentrated_is_high() {
        assert!(approx(gini(&[1.0, 1.0, 1.0, 1.0]), 0.0));
        // All mass in one of four buckets → (n-1)/n = 0.75.
        assert!(approx(gini(&[10.0, 0.0, 0.0, 0.0]), 0.75));
        assert!(approx(gini(&[5.0]), 0.0));
        assert!(approx(gini(&[]), 0.0));
    }

    #[test]
    fn herfindahl_even_vs_monopoly() {
        assert!(approx(herfindahl(&[1.0, 1.0, 1.0, 1.0]), 0.25));
        assert!(approx(herfindahl(&[10.0]), 1.0));
        assert!(approx(herfindahl(&[]), 0.0));
    }

    #[test]
    fn bus_factor_counts_top_contributors() {
        // One author owns everything → bus factor 1.
        assert_eq!(bus_factor(&[10.0, 0.0, 0.0], 0.5), 1);
        // Four equal authors, need ≥50% → 2.
        assert_eq!(bus_factor(&[1.0, 1.0, 1.0, 1.0], 0.5), 2);
        // Need ≥100% of four equal → all 4.
        assert_eq!(bus_factor(&[1.0, 1.0, 1.0, 1.0], 1.0), 4);
        assert_eq!(bus_factor(&[], 0.5), 0);
    }

    #[test]
    fn cosine_orthogonal_and_identical() {
        let a = [1.0f32, 0.0, 0.0];
        let b = [0.0f32, 1.0, 0.0];
        assert!((cosine(&a, &b) - 0.0).abs() < 1e-6);
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-6);
        // Length mismatch → 0.
        assert!((cosine(&a, &[1.0f32, 0.0]) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn l2_normalize_unit_length() {
        let mut v = [3.0f32, 4.0];
        l2_normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
        let mut z = [0.0f32, 0.0];
        l2_normalize(&mut z); // zero stays zero, no NaN
        assert!(z[0] == 0.0 && z[1] == 0.0);
    }

    #[test]
    fn jsd_identical_zero_disjoint_one() {
        assert!(approx(
            js_divergence(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]),
            0.0
        ));
        // Disjoint support → 1.0 (base-2).
        assert!(approx(js_divergence(&[1.0, 0.0], &[0.0, 1.0]), 1.0));
        // Unnormalized but proportional → still identical.
        assert!(approx(js_divergence(&[2.0, 2.0], &[5.0, 5.0]), 0.0));
        // Empty / degenerate → max divergence.
        assert!(approx(js_divergence(&[], &[]), 1.0));
        assert!(approx(js_divergence(&[0.0, 0.0], &[1.0, 1.0]), 1.0));
    }
}
