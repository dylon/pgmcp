//! Retrieval re-ranking extensions (graph-roadmap Phase 4.2): Maximal Marginal
//! Relevance diversity + a recency decay. Pure and dependency-free; the API
//! search handler applies them over the small post-fusion candidate set.
//!
//! - **MMR** (Carbonell & Goldstein, SIGIR 1998): greedily pick results that
//!   are relevant *and* dissimilar to those already picked, so the tiny context
//!   budget isn't spent on near-duplicate chunks. `λ` trades relevance (1.0) for
//!   diversity (0.0).
//! - **Recency multiplier**: an exponential half-life decay on a chunk's last
//!   change date, so fresher code is favored when relevance ties (Rahman-style
//!   churn/recency prior). Half-life 0 ⇒ disabled (multiplier 1.0).

/// Cosine similarity of two equal-length, finite vectors. 0 if either is empty
/// or has zero norm.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom <= 0.0 { 0.0 } else { dot / denom }
}

/// Maximal Marginal Relevance selection. Given candidate `embeddings` (one per
/// candidate, same order as `relevances`, higher = more relevant) returns up to
/// `k` candidate indices ordered by MMR: each pick maximizes
/// `λ·rel − (1−λ)·max cosine to an already-picked candidate`.
///
/// `lambda` is clamped to [0,1]. Relevances are min-max normalized internally so
/// they're comparable to the cosine term. Candidates with a missing/empty
/// embedding still get selected by pure relevance (their diversity penalty is 0).
pub fn mmr_select(
    embeddings: &[Vec<f32>],
    relevances: &[f64],
    lambda: f64,
    k: usize,
) -> Vec<usize> {
    let n = embeddings.len().min(relevances.len());
    if n == 0 || k == 0 {
        return Vec::new();
    }
    let lambda = lambda.clamp(0.0, 1.0) as f32;

    // Min-max normalize relevances to [0,1] for commensurability with cosine.
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for &r in &relevances[..n] {
        if r.is_finite() {
            lo = lo.min(r);
            hi = hi.max(r);
        }
    }
    let span = (hi - lo).max(1e-12);
    let rel = |i: usize| -> f32 {
        if !relevances[i].is_finite() {
            0.0
        } else {
            ((relevances[i] - lo) / span) as f32
        }
    };

    let want = k.min(n);
    let mut selected: Vec<usize> = Vec::with_capacity(want);
    let mut remaining: Vec<usize> = (0..n).collect();

    while selected.len() < want && !remaining.is_empty() {
        let mut best_pos = 0usize;
        let mut best_score = f32::NEG_INFINITY;
        for (pos, &cand) in remaining.iter().enumerate() {
            let max_sim = selected
                .iter()
                .map(|&s| cosine(&embeddings[cand], &embeddings[s]))
                .fold(0.0f32, f32::max);
            let score = lambda * rel(cand) - (1.0 - lambda) * max_sim;
            if score > best_score {
                best_score = score;
                best_pos = pos;
            }
        }
        selected.push(remaining.swap_remove(best_pos));
    }
    selected
}

/// Exponential recency multiplier in (0, 1]: `0.5 ^ (age_days / half_life_days)`.
/// `half_life_days <= 0` disables it (returns 1.0); negative ages clamp to 0.
pub fn recency_multiplier(age_days: f64, half_life_days: f64) -> f64 {
    if half_life_days <= 0.0 {
        return 1.0;
    }
    let age = age_days.max(0.0);
    0.5f64.powf(age / half_life_days)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mmr_dedupes_near_duplicates() {
        // 0 and 1 are identical; 2 is orthogonal. Relevances favor 0 then 1.
        let embs = vec![
            vec![1.0, 0.0],
            vec![1.0, 0.0], // duplicate of 0
            vec![0.0, 1.0], // diverse
        ];
        let rels = vec![1.0, 0.9, 0.5];
        // Pure relevance (λ=1) would pick 0,1,2. MMR (λ=0.5) should prefer the
        // diverse 2 over the duplicate 1 for the second slot.
        let picked = mmr_select(&embs, &rels, 0.5, 2);
        assert_eq!(picked[0], 0, "most relevant first");
        assert_eq!(picked[1], 2, "diversity beats the near-duplicate");
    }

    #[test]
    fn mmr_lambda_one_is_pure_relevance_order() {
        let embs = vec![vec![1.0, 0.0], vec![1.0, 0.0], vec![0.0, 1.0]];
        let rels = vec![0.2, 0.9, 0.5];
        let picked = mmr_select(&embs, &rels, 1.0, 3);
        assert_eq!(picked, vec![1, 2, 0], "λ=1 ⇒ descending relevance");
    }

    #[test]
    fn mmr_edge_cases() {
        assert!(mmr_select(&[], &[], 0.5, 5).is_empty());
        assert!(mmr_select(&[vec![1.0]], &[1.0], 0.5, 0).is_empty());
    }

    #[test]
    fn recency_decay_halves_each_half_life() {
        assert!((recency_multiplier(0.0, 30.0) - 1.0).abs() < 1e-9);
        assert!((recency_multiplier(30.0, 30.0) - 0.5).abs() < 1e-9);
        assert!((recency_multiplier(60.0, 30.0) - 0.25).abs() < 1e-9);
        assert_eq!(recency_multiplier(100.0, 0.0), 1.0, "disabled");
    }
}
