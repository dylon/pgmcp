//! Retrieval-quality metrics — recall@k, precision@k, MRR, MAP, and graded
//! nDCG@k over a ranked result list against a labeled gold set.
//!
//! ## Why this module exists
//!
//! pgmcp embeds ~644k chunks with BGE-M3 and ranks them with pgvector HNSW
//! cosine, but until now there was **no way to measure whether that ranking is
//! good**: the benchmarks measured latency only, and the tests asserted
//! *findability* (binary "is the row present at all") or *RRF-formula*
//! correctness — never recall@k / MRR / nDCG against labeled relevance. This
//! module is the missing yardstick: the pure, closed-form metric math that the
//! retrieval-quality evaluation campaign (`pgmcp-testing` `eval_retrieval` bin)
//! computes per query, then feeds — as per-query vectors — to the paired
//! statistics in [`crate::stats::inference`].
//!
//! ## Design: separate *matching* from *metric math*
//!
//! Every metric is a pure function of a **gain vector** `gains[i]` = the graded
//! relevance of the result at rank `i+1` (0 if the result matches no gold
//! item), plus the **ideal gain vector** = the gold relevances sorted
//! descending. [`gain_vector`] / [`ideal_gains`] perform the matching once; the
//! metric functions ([`dcg_at_k`], [`ndcg_at_k`], …) consume only the numbers.
//! This keeps the metric math trivially unit-testable with hand-computed
//! closed-form values, independent of any corpus or search backend.
//!
//! ## Granularity
//!
//! A retrieved chunk matches a gold item either at [`MatchGranularity::File`]
//! (same `path`) or [`MatchGranularity::Chunk`] (same `path` **and** the line
//! spans overlap). The campaign reports both: file-granularity is the uniform
//! cross-mode key (a chunk-granularity tool and a file-granularity one become
//! comparable), chunk-granularity is the stricter within-chunk-mode check.
//!
//! ## Score-agnostic by construction
//!
//! Every metric here is derived from **ranks**, never from absolute scores.
//! This is deliberate: cosine similarities for BGE-M3 sit in a tight ~0.56–0.68
//! band (high-dimensional distance concentration), so an absolute-threshold
//! metric would be brittle, and the fused RRF scores of `hybrid_search` (~0.008)
//! are not comparable to cosine at all. Ranking is the signal.
//!
//! ## References
//!
//! - Järvelin, K. & Kekäläinen, J. (2002). *Cumulated gain-based evaluation of
//!   IR techniques.* ACM TOIS 20(4). (nDCG). doi:10.1145/582415.582418
//! - Manning, Raghavan & Schütze (2008). *Introduction to Information
//!   Retrieval*, ch. 8. (MAP, MRR, precision/recall@k.)
//! - Husain et al. (2019). *CodeSearchNet Challenge.* arXiv:1909.09436.
//!   (docstring-as-query MRR for code retrieval — the campaign's ground truth.)

// This module is exercised primarily from the `pgmcp-testing` campaign bin and
// the unit tests below; the per-metric free functions look dead to the main
// binary's reachability analysis. Mirror the module-level allow used elsewhere
// in `quality`.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// Granularity at which a retrieved hit is judged to match a gold item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchGranularity {
    /// Match iff the `path` is equal. The uniform cross-mode key.
    File,
    /// Match iff the `path` is equal **and** the `[start_line, end_line]`
    /// spans overlap. The stricter within-chunk-mode key. A hit or gold item
    /// with no line span (`None`) falls back to file-level for that item.
    Chunk,
}

/// One retrieved result, reduced to the fields the metrics need. Built by the
/// campaign from a `SearchResult` / hybrid row. `score` is retained only for
/// the score-margin diagnostic — no metric depends on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankedHit {
    pub path: String,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub score: Option<f64>,
}

impl RankedHit {
    /// Construct a file-only hit (no line span), e.g. for file-granularity
    /// modes or quick fixtures.
    pub fn path_only(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            start_line: None,
            end_line: None,
            score: None,
        }
    }
}

/// One gold (relevant) item for a query. `relevance` is the graded gain — `1.0`
/// for a binary task, or `{1,2,3}` for graded relevance. Multiple gold items
/// per query are supported (e.g. a docstring's code unit spanning >1 chunk).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoldItem {
    pub path: String,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub relevance: f64,
}

impl GoldItem {
    /// A binary-relevant (gain 1.0) gold item identified by path only.
    pub fn path_only(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            start_line: None,
            end_line: None,
            relevance: 1.0,
        }
    }
}

/// The metric values at a single `k` cut-off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtK {
    pub k: usize,
    /// Fraction of distinct gold items matched within the top-`k`.
    pub recall: f64,
    /// Relevant positions in the top-`k`, divided by `k`.
    pub precision: f64,
    /// `1.0` if any gold matched within the top-`k`, else `0.0`.
    pub success: f64,
    /// Normalized discounted cumulative gain at `k` (graded), in `[0, 1]`.
    pub ndcg: f64,
}

/// The full metric row for one query against one search mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryMetrics {
    /// `1 / rank` of the first relevant hit (1-indexed); `0.0` if none.
    pub reciprocal_rank: f64,
    /// Average precision, denominator = number of gold items.
    pub average_precision: f64,
    /// 1-indexed rank of the first relevant hit, `None` if no gold retrieved.
    pub first_relevant_rank: Option<usize>,
    /// Per-`k` metrics for the requested cut-offs.
    pub at_k: Vec<AtK>,
}

impl QueryMetrics {
    /// The `recall` value at a given `k` (panics if `k` was not requested).
    pub fn recall_at(&self, k: usize) -> f64 {
        self.at_k
            .iter()
            .find(|a| a.k == k)
            .map(|a| a.recall)
            .unwrap_or_else(|| panic!("recall@{k} was not computed"))
    }

    /// The `ndcg` value at a given `k` (panics if `k` was not requested).
    pub fn ndcg_at(&self, k: usize) -> f64 {
        self.at_k
            .iter()
            .find(|a| a.k == k)
            .map(|a| a.ndcg)
            .unwrap_or_else(|| panic!("ndcg@{k} was not computed"))
    }
}

// ============================================================================
// Matching: ranked hits → gain vectors
// ============================================================================

/// Two line spans overlap iff `a_start ≤ b_end ∧ b_start ≤ a_end`. A `None`
/// endpoint is treated as unbounded on that side (so a span-less item overlaps
/// any span on the same path — the file-level fallback).
fn spans_overlap(
    a_start: Option<i64>,
    a_end: Option<i64>,
    b_start: Option<i64>,
    b_end: Option<i64>,
) -> bool {
    // If either item lacks a span entirely, fall back to "overlaps".
    if (a_start.is_none() && a_end.is_none()) || (b_start.is_none() && b_end.is_none()) {
        return true;
    }
    let a_lo = a_start.unwrap_or(i64::MIN);
    let a_hi = a_end.unwrap_or(i64::MAX);
    let b_lo = b_start.unwrap_or(i64::MIN);
    let b_hi = b_end.unwrap_or(i64::MAX);
    a_lo <= b_hi && b_lo <= a_hi
}

/// Does this hit match this gold item at the given granularity?
fn matches(hit: &RankedHit, gold: &GoldItem, gran: MatchGranularity) -> bool {
    if hit.path != gold.path {
        return false;
    }
    match gran {
        MatchGranularity::File => true,
        MatchGranularity::Chunk => {
            spans_overlap(hit.start_line, hit.end_line, gold.start_line, gold.end_line)
        }
    }
}

/// Collapse a ranked list to first-occurrence-per-`path`, preserving order.
///
/// This is the dedup the campaign applies to **every** mode before scoring.
/// `hybrid_search` in particular emits the same file once per fused leg
/// (dense + lexical + sparse); scoring those duplicates would distort recall
/// and precision. `path` is the uniform key because `semantic_search` results
/// carry no chunk id.
pub fn path_dedup(hits: &[RankedHit]) -> Vec<RankedHit> {
    let mut seen: std::collections::HashSet<&str> =
        std::collections::HashSet::with_capacity(hits.len());
    let mut out: Vec<RankedHit> = Vec::with_capacity(hits.len());
    for h in hits {
        if seen.insert(h.path.as_str()) {
            out.push(h.clone());
        }
    }
    out
}

/// Per-position graded gain: `gains[i]` = the maximum relevance over all gold
/// items the hit at rank `i+1` matches, or `0.0` if it matches none.
pub fn gain_vector(ranked: &[RankedHit], gold: &[GoldItem], gran: MatchGranularity) -> Vec<f64> {
    ranked
        .iter()
        .map(|hit| {
            gold.iter()
                .filter(|g| matches(hit, g, gran))
                .map(|g| g.relevance)
                .fold(0.0_f64, f64::max)
        })
        .collect()
}

/// The ideal gain vector: gold relevances sorted descending. Used as the IDCG
/// numerator for nDCG.
pub fn ideal_gains(gold: &[GoldItem]) -> Vec<f64> {
    let mut g: Vec<f64> = gold.iter().map(|x| x.relevance).collect();
    g.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    g
}

/// Number of **distinct** gold items matched within the top-`k` of `ranked`.
/// (Distinct gold, not distinct positions — so duplicate hits or one file
/// covering several gold spans are each counted once on the gold side.)
fn matched_gold_in_top_k(
    ranked: &[RankedHit],
    gold: &[GoldItem],
    gran: MatchGranularity,
    k: usize,
) -> usize {
    let mut hit_gold = vec![false; gold.len()];
    for hit in ranked.iter().take(k) {
        for (j, g) in gold.iter().enumerate() {
            if !hit_gold[j] && matches(hit, g, gran) {
                hit_gold[j] = true;
            }
        }
    }
    hit_gold.iter().filter(|x| **x).count()
}

// ============================================================================
// Pure metric math (operate on gain vectors only)
// ============================================================================

/// Discounted cumulative gain at `k`: `Σ_{i=1..k} gain_i / log₂(i + 1)`.
pub fn dcg_at_k(gains: &[f64], k: usize) -> f64 {
    gains
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, &g)| {
            // rank = i + 1 (1-indexed) ⇒ discount log₂(rank + 1) = log₂(i + 2).
            g / ((i + 2) as f64).log2()
        })
        .sum()
}

/// Normalized DCG at `k`: `DCG@k(gains) / DCG@k(ideal)`, `0.0` when the ideal
/// is empty (no relevant items).
pub fn ndcg_at_k(gains: &[f64], ideal: &[f64], k: usize) -> f64 {
    let idcg = dcg_at_k(ideal, k);
    if idcg <= 0.0 {
        return 0.0;
    }
    dcg_at_k(gains, k) / idcg
}

/// Precision at `k`: relevant positions in the top-`k`, divided by `k`. Uses
/// the fixed denominator `k` (the standard convention; a short list is
/// penalized for the empty tail).
pub fn precision_at_k(gains: &[f64], k: usize) -> f64 {
    if k == 0 {
        return 0.0;
    }
    let rel = gains.iter().take(k).filter(|&&g| g > 0.0).count();
    rel as f64 / k as f64
}

/// `1.0` if any of the top-`k` gains is positive, else `0.0`.
pub fn success_at_k(gains: &[f64], k: usize) -> f64 {
    if gains.iter().take(k).any(|&g| g > 0.0) {
        1.0
    } else {
        0.0
    }
}

/// 1-indexed rank of the first positive gain, or `None`.
pub fn first_relevant_rank(gains: &[f64]) -> Option<usize> {
    gains.iter().position(|&g| g > 0.0).map(|i| i + 1)
}

/// Reciprocal rank: `1 / first_relevant_rank`, or `0.0` if none.
pub fn reciprocal_rank(gains: &[f64]) -> f64 {
    first_relevant_rank(gains).map_or(0.0, |r| 1.0 / r as f64)
}

/// Average precision: `(1/R) Σ_k [rel_k · Precision@k]` over the full ranked
/// list, where `R = total_relevant`. Returns `0.0` when `total_relevant == 0`.
///
/// `total_relevant` is the number of gold items (the recall-aware denominator),
/// so an unretrieved gold item correctly drags AP down.
pub fn average_precision(gains: &[f64], total_relevant: usize) -> f64 {
    if total_relevant == 0 {
        return 0.0;
    }
    let mut hits = 0usize;
    let mut sum = 0.0_f64;
    for (i, &g) in gains.iter().enumerate() {
        if g > 0.0 {
            hits += 1;
            // precision at this 1-indexed rank (i + 1).
            sum += hits as f64 / (i + 1) as f64;
        }
    }
    sum / total_relevant as f64
}

// ============================================================================
// Per-query assembly
// ============================================================================

/// Compute the full [`QueryMetrics`] for one ranked list against one gold set
/// at the requested `k` cut-offs.
///
/// `ranked` should already be path-deduped ([`path_dedup`]). Returns `NaN`
/// recall for the degenerate empty-gold case (the caller should not score
/// queries with no gold).
pub fn compute_query_metrics(
    ranked: &[RankedHit],
    gold: &[GoldItem],
    gran: MatchGranularity,
    ks: &[usize],
) -> QueryMetrics {
    let gains = gain_vector(ranked, gold, gran);
    let ideal = ideal_gains(gold);
    let total_relevant = gold.len();

    let at_k = ks
        .iter()
        .map(|&k| {
            let recall = if total_relevant == 0 {
                f64::NAN
            } else {
                matched_gold_in_top_k(ranked, gold, gran, k) as f64 / total_relevant as f64
            };
            AtK {
                k,
                recall,
                precision: precision_at_k(&gains, k),
                success: success_at_k(&gains, k),
                ndcg: ndcg_at_k(&gains, &ideal, k),
            }
        })
        .collect();

    QueryMetrics {
        reciprocal_rank: reciprocal_rank(&gains),
        average_precision: average_precision(&gains, total_relevant),
        first_relevant_rank: first_relevant_rank(&gains),
        at_k,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hits(paths: &[&str]) -> Vec<RankedHit> {
        paths.iter().map(|p| RankedHit::path_only(*p)).collect()
    }

    const KS: [usize; 4] = [1, 5, 10, 20];

    #[test]
    fn perfect_ranking_scores_one() {
        // Single gold "a", retrieved at rank 1.
        let ranked = hits(&["a", "b", "c"]);
        let gold = vec![GoldItem::path_only("a")];
        let m = compute_query_metrics(&ranked, &gold, MatchGranularity::File, &KS);
        assert_eq!(m.reciprocal_rank, 1.0);
        assert_eq!(m.average_precision, 1.0);
        assert_eq!(m.first_relevant_rank, Some(1));
        for a in &m.at_k {
            assert_eq!(a.recall, 1.0, "recall@{}", a.k);
            assert_eq!(a.success, 1.0, "success@{}", a.k);
            assert!((a.ndcg - 1.0).abs() < 1e-12, "ndcg@{}={}", a.k, a.ndcg);
        }
    }

    #[test]
    fn gold_absent_scores_zero() {
        let ranked = hits(&["x", "y", "z"]);
        let gold = vec![GoldItem::path_only("a")];
        let m = compute_query_metrics(&ranked, &gold, MatchGranularity::File, &KS);
        assert_eq!(m.reciprocal_rank, 0.0);
        assert_eq!(m.average_precision, 0.0);
        assert_eq!(m.first_relevant_rank, None);
        for a in &m.at_k {
            assert_eq!(a.recall, 0.0, "recall@{}", a.k);
            assert_eq!(a.success, 0.0, "success@{}", a.k);
            assert_eq!(a.ndcg, 0.0, "ndcg@{}", a.k);
        }
    }

    #[test]
    fn mrr_reciprocal_of_first_relevant_rank() {
        // Gold "a" at rank 3 of [b, c, a, d, e].
        let ranked = hits(&["b", "c", "a", "d", "e"]);
        let gold = vec![GoldItem::path_only("a")];
        let gains = gain_vector(&ranked, &gold, MatchGranularity::File);
        assert_eq!(gains, vec![0.0, 0.0, 1.0, 0.0, 0.0]);

        let m = compute_query_metrics(&ranked, &gold, MatchGranularity::File, &KS);
        assert!((m.reciprocal_rank - 1.0 / 3.0).abs() < 1e-12);
        assert_eq!(m.first_relevant_rank, Some(3));
        assert_eq!(m.recall_at(1), 0.0);
        assert_eq!(m.recall_at(5), 1.0);
        // nDCG@10 = (1/log2(4)) / (1/log2(2)) = 0.5 / 1 = 0.5.
        assert!((m.ndcg_at(10) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn precision_recall_ap_multi_gold() {
        // Two gold {a, c}, retrieved at ranks 1 and 3 of [a, b, c, d, e].
        let ranked = hits(&["a", "b", "c", "d", "e"]);
        let gold = vec![GoldItem::path_only("a"), GoldItem::path_only("c")];
        let m = compute_query_metrics(&ranked, &gold, MatchGranularity::File, &KS);

        assert_eq!(m.recall_at(1), 0.5); // {a} of {a,c}
        assert_eq!(m.recall_at(5), 1.0); // {a,c}
        // precision@5 = 2 relevant / 5.
        let p5 = m.at_k.iter().find(|a| a.k == 5).unwrap().precision;
        assert!((p5 - 2.0 / 5.0).abs() < 1e-12);
        // AP = (P@1 + P@3)/2 = (1/1 + 2/3)/2 = 5/6.
        assert!(
            (m.average_precision - 5.0 / 6.0).abs() < 1e-12,
            "ap={}",
            m.average_precision
        );
    }

    #[test]
    fn ndcg_graded_two_levels() {
        // Graded gold: a=3, b=2. Mis-ordered ranking [b, a].
        let ranked = hits(&["b", "a"]);
        let gold = vec![
            GoldItem {
                path: "a".into(),
                start_line: None,
                end_line: None,
                relevance: 3.0,
            },
            GoldItem {
                path: "b".into(),
                start_line: None,
                end_line: None,
                relevance: 2.0,
            },
        ];
        let gains = gain_vector(&ranked, &gold, MatchGranularity::File);
        assert_eq!(gains, vec![2.0, 3.0]);
        // DCG@2 = 2/log2(2) + 3/log2(3) = 2 + 3/1.5849625 = 2 + 1.892789 = 3.892789.
        let dcg = dcg_at_k(&gains, 2);
        assert!((dcg - 3.892789).abs() < 1e-5, "dcg={dcg}");
        // IDCG@2 (ideal [3,2]) = 3/1 + 2/1.5849625 = 3 + 1.261859 = 4.261859.
        let idcg = dcg_at_k(&ideal_gains(&gold), 2);
        assert!((idcg - 4.261859).abs() < 1e-5, "idcg={idcg}");
        let ndcg = ndcg_at_k(&gains, &ideal_gains(&gold), 2);
        assert!((ndcg - dcg / idcg).abs() < 1e-12);
        assert!((ndcg - 0.913402).abs() < 1e-5, "ndcg={ndcg}");
    }

    #[test]
    fn path_dedup_keeps_first_occurrence() {
        let raw = vec![
            RankedHit {
                path: "a".into(),
                start_line: Some(1),
                end_line: Some(9),
                score: Some(0.9),
            },
            RankedHit {
                path: "a".into(),
                start_line: Some(20),
                end_line: Some(29),
                score: Some(0.8),
            },
            RankedHit {
                path: "b".into(),
                start_line: None,
                end_line: None,
                score: Some(0.7),
            },
        ];
        let deduped = path_dedup(&raw);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].start_line, Some(1)); // first "a" kept
        assert_eq!(deduped[1].path, "b");
    }

    #[test]
    fn chunk_granularity_requires_span_overlap() {
        // Same path, but the hit span [1,9] does not overlap gold span [20,29].
        let ranked = vec![RankedHit {
            path: "a".into(),
            start_line: Some(1),
            end_line: Some(9),
            score: Some(0.9),
        }];
        let gold = vec![GoldItem {
            path: "a".into(),
            start_line: Some(20),
            end_line: Some(29),
            relevance: 1.0,
        }];
        // File granularity: path matches → relevant.
        assert_eq!(
            success_at_k(&gain_vector(&ranked, &gold, MatchGranularity::File), 1),
            1.0
        );
        // Chunk granularity: spans disjoint → not relevant.
        assert_eq!(
            success_at_k(&gain_vector(&ranked, &gold, MatchGranularity::Chunk), 1),
            0.0
        );
    }

    #[test]
    fn chunk_granularity_overlapping_span_matches() {
        let ranked = vec![RankedHit {
            path: "a".into(),
            start_line: Some(15),
            end_line: Some(25),
            score: Some(0.9),
        }];
        let gold = vec![GoldItem {
            path: "a".into(),
            start_line: Some(20),
            end_line: Some(29),
            relevance: 1.0,
        }];
        assert_eq!(
            success_at_k(&gain_vector(&ranked, &gold, MatchGranularity::Chunk), 1),
            1.0
        );
    }

    #[test]
    fn k_larger_than_list_is_graceful() {
        let ranked = hits(&["a", "b"]);
        let gold = vec![GoldItem::path_only("a")];
        let m = compute_query_metrics(&ranked, &gold, MatchGranularity::File, &[20]);
        // recall@20 = 1 (a found), precision@20 = 1/20 (one relevant, fixed denom).
        assert_eq!(m.recall_at(20), 1.0);
        let p20 = m.at_k[0].precision;
        assert!((p20 - 1.0 / 20.0).abs() < 1e-12, "p@20={p20}");
    }

    #[test]
    fn empty_gold_yields_nan_recall_not_panic() {
        let ranked = hits(&["a"]);
        let gold: Vec<GoldItem> = Vec::new();
        let m = compute_query_metrics(&ranked, &gold, MatchGranularity::File, &[10]);
        assert!(m.recall_at(10).is_nan());
        assert_eq!(m.reciprocal_rank, 0.0);
    }
}
