//! Topic-quality metrics — coherence, diversity, cluster validity, and
//! degeneracy detection for the topic-clustering pipeline.
//!
//! ## Why this module exists
//!
//! On 2026-06-13 the global topic model was found to have *silently collapsed*:
//! all 200 topics shared the label `the / and / dylon / home / workspace`, every
//! chunk was assigned to ~199 of 200 topics, and the fuzzy memberships were
//! uniform (≈ 1/K). The breakage had gone unnoticed for ~3 weeks because **no
//! topic-quality signal was ever computed or persisted** — the K-selector
//! computed Xie-Beni / fuzzy-silhouette during the K sweep and then *discarded*
//! them, and `store_topics` only inspected labels *after* it had already cleared
//! and overwritten the previous (good) topics.
//!
//! This module is the yardstick **and** the alarm:
//!
//! 1. It scores any clustering result identically, so the Phase-3 bake-off can
//!    rank engines (graph-hybrid vs embedding-BERTopic vs the FCM baseline) on
//!    the same axes.
//! 2. It exposes a [`TopicMetrics::degeneracy_reason`] gate the scan paths
//!    consult **before** overwriting good topics, so a collapse can never again
//!    silently replace a healthy model.
//! 3. It is persisted (`db::queries::topics::set_topic_quality`) and surfaced in
//!    `orient` / the digest, so regressions are visible.
//!
//! ## Metrics
//!
//! - **mean_max_membership** = `mean_i max_j μ_ij`. For K clusters, uniform
//!   (collapsed) memberships give `1/K`; a crisp assignment approaches `1`. This
//!   is the single cheapest, most direct detector of the FCM collapse.
//! - **fuzzy_silhouette** / **xie_beni** — cluster-validity indices (reused from
//!   [`crate::cron::k_selector`]), computed on the *final* model on a bounded
//!   subsample (not just the K-sweep candidate).
//! - **topics_per_doc_mean** / **max_topic_share** — assignment-spread signals
//!   (the "198-topics-per-chunk smearing" and the "one mega-topic absorbs the
//!   whole corpus" pathologies, respectively).
//! - **distinct_label_ratio** / **topic_diversity** — label-collapse detectors.
//! - **npmi_coherence** / **umass_coherence** — topic coherence over each topic's
//!   top terms: NPMI is the OCTIS/gensim-standard `c_npmi` (document-level
//!   co-occurrence), UMass is the classic intrinsic cross-check. Filled only when
//!   document text is supplied (the in-memory + bake-off paths); left `NaN`
//!   (serialised as `null`) on the streaming global path, which gates on the
//!   cheaper structural signals above.

use ndarray::{ArrayView2, s};

use crate::cron::topic_clustering::{TopicResult, tokenize_into};

/// Cap on the number of rows fed to the O(n·K·d) validity indices. The full
/// global corpus is ~600k chunks; computing Xie-Beni over all of it would be
/// ~6×10¹⁰ FLOPs per scan. 20k rows is a statistically ample sample for a
/// stable index value and keeps the metric well under a second.
const VALIDITY_SUBSAMPLE_CAP: usize = 20_000;

/// Default number of leading keywords per topic used for coherence + diversity.
pub const DEFAULT_COHERENCE_TOP_N: usize = 10;

/// A computed snapshot of one clustering result's quality.
///
/// `NaN` fields mean "not computed on this path" and serialise to JSON `null`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TopicMetrics {
    /// Number of clusters K the clusterer was asked for.
    pub k: usize,
    /// Number of non-empty topics actually produced.
    pub n_topics: usize,
    /// Number of documents (chunks) scored for the geometry metrics.
    pub n_scored: usize,

    // ── geometry / membership ────────────────────────────────────────────
    /// `mean_i max_j μ_ij`. → `1/K` ⇒ collapsed; → `1` ⇒ crisp.
    pub mean_max_membership: f64,
    /// Fuzzy silhouette in [-1, 1] (higher is better).
    pub fuzzy_silhouette: f64,
    /// Xie-Beni index (lower is better; `+inf` is degenerate).
    pub xie_beni: f64,

    // ── assignment spread ────────────────────────────────────────────────
    /// Mean number of topics each (assigned) document belongs to.
    pub topics_per_doc_mean: f64,
    /// Largest topic's share of all assignments, in [0, 1].
    pub max_topic_share: f64,

    // ── label / coherence ────────────────────────────────────────────────
    /// `distinct(labels) / n_topics`, in [0, 1].
    pub distinct_label_ratio: f64,
    /// `unique(top terms) / total(top terms)`, in [0, 1] (topic diversity).
    pub topic_diversity: f64,
    /// Mean NPMI coherence over topics' top terms, in [-1, 1] (`NaN` if no docs).
    pub npmi_coherence: f64,
    /// Mean UMass coherence (≤ 0; closer to 0 is better; `NaN` if no docs).
    pub umass_coherence: f64,
    /// Graph modularity Q of the partition, in [-0.5, 1] (`NaN` for the
    /// embedding tracks; set by the graph-hybrid track from Louvain/Leiden).
    pub modularity: f64,
}

impl TopicMetrics {
    /// Compute the geometry + label metrics from a final clustering model.
    ///
    /// Coherence (`npmi_coherence` / `umass_coherence`) is left `NaN`; call
    /// [`TopicMetrics::fill_coherence`] with the document text to populate it.
    ///
    /// `membership` is `(n × k)` and `data` / `centroids` are `(n × d)` /
    /// `(k × d)`; the validity indices run on the first
    /// [`VALIDITY_SUBSAMPLE_CAP`] rows to bound cost.
    pub fn compute(
        data: ArrayView2<f32>,
        membership: ArrayView2<f32>,
        centroids: ArrayView2<f32>,
        m: f64,
        k: usize,
        topics: &[TopicResult],
    ) -> Self {
        let n = membership.nrows();

        // mean_i max_j μ_ij — the collapse detector. Computed on the full set
        // (O(n·K), cheap) since it is the load-bearing gate signal.
        let mean_max_membership = if n == 0 {
            0.0
        } else {
            let mut acc = 0.0f64;
            for i in 0..n {
                let row = membership.row(i);
                let mx = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                if mx.is_finite() {
                    acc += mx as f64;
                }
            }
            acc / n as f64
        };

        // Validity indices on a bounded subsample (views, no copy).
        let sub_n = n.min(VALIDITY_SUBSAMPLE_CAP);
        let (fuzzy_silhouette, xie_beni) = if sub_n >= 2 && k >= 2 {
            let sd = data.slice(s![..sub_n, ..]);
            let sm = membership.slice(s![..sub_n, ..]);
            (
                crate::cron::k_selector::fuzzy_silhouette(sd, sm, centroids, 1.0),
                crate::cron::k_selector::xie_beni(sd, sm, centroids, m),
            )
        } else {
            (0.0, f64::INFINITY)
        };

        let (topics_per_doc_mean, max_topic_share) = assignment_spread(topics);
        let distinct_label_ratio = distinct_label_ratio(topics);
        let topic_diversity = topic_diversity(topics, DEFAULT_COHERENCE_TOP_N);

        Self {
            k,
            n_topics: topics.iter().filter(|t| !t.chunk_ids.is_empty()).count(),
            n_scored: sub_n,
            mean_max_membership,
            fuzzy_silhouette,
            xie_beni,
            topics_per_doc_mean,
            max_topic_share,
            distinct_label_ratio,
            topic_diversity,
            npmi_coherence: f64::NAN,
            umass_coherence: f64::NAN,
            modularity: f64::NAN,
        }
    }

    /// A label/coherence-only snapshot for paths that lack the membership
    /// matrix (e.g. graph-community topics where assignment is a hard
    /// partition). Geometry fields are filled from `topics` where possible and
    /// the membership-only fields are left `NaN`.
    pub fn from_topics(k: usize, topics: &[TopicResult]) -> Self {
        let (topics_per_doc_mean, max_topic_share) = assignment_spread(topics);
        Self {
            k,
            n_topics: topics.iter().filter(|t| !t.chunk_ids.is_empty()).count(),
            n_scored: 0,
            mean_max_membership: f64::NAN,
            fuzzy_silhouette: f64::NAN,
            xie_beni: f64::NAN,
            topics_per_doc_mean,
            max_topic_share,
            distinct_label_ratio: distinct_label_ratio(topics),
            topic_diversity: topic_diversity(topics, DEFAULT_COHERENCE_TOP_N),
            npmi_coherence: f64::NAN,
            umass_coherence: f64::NAN,
            modularity: f64::NAN,
        }
    }

    /// Populate `npmi_coherence` + `umass_coherence` from the corpus text.
    ///
    /// `documents` is a sample of the chunk contents; `top_n` is the number of
    /// leading keywords per topic to score. One streaming pass tokenises each
    /// document (with the same [`tokenize_into`] the labels were derived from),
    /// restricts to the union of topic top-terms, and accumulates document- and
    /// co-document-frequencies, from which NPMI / UMass are derived.
    pub fn fill_coherence(&mut self, documents: &[&str], topics: &[TopicResult], top_n: usize) {
        let (npmi, umass) = coherence(documents, topics, top_n);
        self.npmi_coherence = npmi;
        self.umass_coherence = umass;
    }

    /// Return a human-readable reason if this model is degenerate per
    /// `thresholds`, else `None`. Used as the pre-overwrite gate.
    ///
    /// Order matters only for which reason is reported first; any single failed
    /// check marks the model degenerate.
    pub fn degeneracy_reason(&self, thresholds: &DegeneracyThresholds) -> Option<String> {
        let k = self.k.max(1) as f64;

        // mean_max_membership floor scales with K: uniform memberships are 1/K,
        // so we require at least `factor × (1/K)`.
        if self.mean_max_membership.is_finite() {
            let floor = thresholds.min_mean_max_membership_factor / k;
            if self.mean_max_membership < floor {
                return Some(format!(
                    "mean_max_membership {:.4} < floor {:.4} (≈{:.1}/K); memberships are ~uniform (collapsed FCM)",
                    self.mean_max_membership, floor, thresholds.min_mean_max_membership_factor
                ));
            }
        }

        // Label collapse — only meaningful with enough topics to compare.
        if self.n_topics >= 5 && self.distinct_label_ratio < thresholds.min_distinct_label_ratio {
            return Some(format!(
                "distinct_label_ratio {:.3} < {:.3}; topic labels collapsed onto a few repeats",
                self.distinct_label_ratio, thresholds.min_distinct_label_ratio
            ));
        }

        if self.topics_per_doc_mean.is_finite()
            && self.topics_per_doc_mean > thresholds.max_topics_per_doc
        {
            return Some(format!(
                "topics_per_doc_mean {:.2} > {:.2}; chunks smeared across too many topics",
                self.topics_per_doc_mean, thresholds.max_topics_per_doc
            ));
        }

        if self.max_topic_share.is_finite() && self.max_topic_share > thresholds.max_topic_share {
            return Some(format!(
                "max_topic_share {:.3} > {:.3}; one mega-topic absorbs the corpus",
                self.max_topic_share, thresholds.max_topic_share
            ));
        }

        if self.fuzzy_silhouette.is_finite()
            && self.fuzzy_silhouette < thresholds.min_fuzzy_silhouette
        {
            return Some(format!(
                "fuzzy_silhouette {:.3} < {:.3}; clusters not separated",
                self.fuzzy_silhouette, thresholds.min_fuzzy_silhouette
            ));
        }

        None
    }

    /// Convenience predicate over [`TopicMetrics::degeneracy_reason`].
    pub fn is_degenerate(&self, thresholds: &DegeneracyThresholds) -> bool {
        self.degeneracy_reason(thresholds).is_some()
    }

    /// Serialise to a JSON value (NaN → null) for persistence.
    pub fn to_json(&self) -> serde_json::Value {
        // serde_json renders f64::NAN as null only via custom handling; do it
        // explicitly so the stored document is valid JSON.
        let f = |v: f64| -> serde_json::Value {
            if v.is_finite() {
                serde_json::json!(v)
            } else {
                serde_json::Value::Null
            }
        };
        serde_json::json!({
            "k": self.k,
            "n_topics": self.n_topics,
            "n_scored": self.n_scored,
            "mean_max_membership": f(self.mean_max_membership),
            "fuzzy_silhouette": f(self.fuzzy_silhouette),
            "xie_beni": f(self.xie_beni),
            "topics_per_doc_mean": f(self.topics_per_doc_mean),
            "max_topic_share": f(self.max_topic_share),
            "distinct_label_ratio": f(self.distinct_label_ratio),
            "topic_diversity": f(self.topic_diversity),
            "npmi_coherence": f(self.npmi_coherence),
            "umass_coherence": f(self.umass_coherence),
            "modularity": f(self.modularity),
        })
    }
}

/// Thresholds for the degeneracy gate. Build from [`crate::config::CronConfig`]
/// via [`DegeneracyThresholds::from_config`].
#[derive(Debug, Clone)]
pub struct DegeneracyThresholds {
    /// Gate floor for `mean_max_membership` is `factor / K`. Default 2.0 ⇒ a
    /// model whose mean top membership is below twice the uniform `1/K` baseline
    /// is rejected.
    pub min_mean_max_membership_factor: f64,
    /// Minimum distinct-label ratio (default 0.30).
    pub min_distinct_label_ratio: f64,
    /// Maximum mean topics-per-doc (default 6.0; the v3 per-chunk cap is 4).
    pub max_topics_per_doc: f64,
    /// Maximum single-topic corpus share (default 0.60).
    pub max_topic_share: f64,
    /// Minimum fuzzy silhouette (default -1.0 ⇒ effectively disabled; the
    /// membership/label signals are the load-bearing gates).
    pub min_fuzzy_silhouette: f64,
}

impl Default for DegeneracyThresholds {
    fn default() -> Self {
        Self {
            min_mean_max_membership_factor: 2.0,
            min_distinct_label_ratio: 0.30,
            max_topics_per_doc: 6.0,
            max_topic_share: 0.60,
            min_fuzzy_silhouette: -1.0,
        }
    }
}

impl DegeneracyThresholds {
    /// Build from the cron config's `topic_*` gate fields.
    pub fn from_config(cfg: &crate::config::CronConfig) -> Self {
        Self {
            min_mean_max_membership_factor: cfg.topic_min_mean_max_membership_factor,
            min_distinct_label_ratio: cfg.topic_min_distinct_label_ratio,
            max_topics_per_doc: cfg.topic_max_topics_per_doc,
            max_topic_share: cfg.topic_max_topic_share,
            min_fuzzy_silhouette: cfg.topic_min_fuzzy_silhouette,
        }
    }
}

// ============================================================================
// Free functions (label/spread/coherence) — pure, unit-testable
// ============================================================================

/// `(mean topics-per-assigned-doc, largest-topic share of all assignments)`.
fn assignment_spread(topics: &[TopicResult]) -> (f64, f64) {
    let mut total_assignments: u64 = 0;
    let mut max_topic: u64 = 0;
    let mut distinct_docs: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for t in topics {
        let len = t.chunk_ids.len() as u64;
        total_assignments += len;
        if len > max_topic {
            max_topic = len;
        }
        distinct_docs.extend(t.chunk_ids.iter().copied());
    }
    if total_assignments == 0 {
        return (0.0, 0.0);
    }
    let topics_per_doc = if distinct_docs.is_empty() {
        0.0
    } else {
        total_assignments as f64 / distinct_docs.len() as f64
    };
    let max_share = max_topic as f64 / total_assignments as f64;
    (topics_per_doc, max_share)
}

/// `distinct(labels) / n_non_empty_topics`.
fn distinct_label_ratio(topics: &[TopicResult]) -> f64 {
    let non_empty: Vec<&TopicResult> = topics.iter().filter(|t| !t.chunk_ids.is_empty()).collect();
    if non_empty.is_empty() {
        return 0.0;
    }
    let distinct: std::collections::HashSet<&str> =
        non_empty.iter().map(|t| t.label.as_str()).collect();
    distinct.len() as f64 / non_empty.len() as f64
}

/// Topic diversity = `unique top-terms / total top-terms` across all topics
/// (the OCTIS "topic diversity" metric).
fn topic_diversity(topics: &[TopicResult], top_n: usize) -> f64 {
    let mut total = 0usize;
    let mut unique: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for t in topics {
        for w in t.keywords.iter().take(top_n) {
            total += 1;
            unique.insert(w.as_str());
        }
    }
    if total == 0 {
        return 0.0;
    }
    unique.len() as f64 / total as f64
}

/// Compute `(mean NPMI, mean UMass)` coherence over the topics' top terms using
/// document-level co-occurrence in `documents`.
///
/// Returns `(NaN, NaN)` if there are no documents or no usable top-terms.
fn coherence(documents: &[&str], topics: &[TopicResult], top_n: usize) -> (f64, f64) {
    if documents.is_empty() {
        return (f64::NAN, f64::NAN);
    }

    // Vocabulary = union of the first `top_n` keywords across all topics.
    let mut vocab: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for t in topics {
        for w in t.keywords.iter().take(top_n) {
            let next = vocab.len();
            vocab.entry(w.clone()).or_insert(next);
        }
    }
    if vocab.is_empty() {
        return (f64::NAN, f64::NAN);
    }
    let v = vocab.len();

    // Document- and co-document-frequencies over the vocabulary.
    let mut df = vec![0u32; v];
    let mut codf: std::collections::HashMap<(usize, usize), u32> = std::collections::HashMap::new();
    let mut scratch: Vec<String> = Vec::with_capacity(256);
    let mut present: Vec<usize> = Vec::with_capacity(64);
    let n_docs = documents.len() as f64;

    for doc in documents {
        tokenize_into(doc, &mut scratch);
        present.clear();
        // Restrict to vocabulary, dedup (document presence, not count).
        let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for tok in &scratch {
            if let Some(&idx) = vocab.get(tok)
                && seen.insert(idx)
            {
                present.push(idx);
            }
        }
        for &idx in &present {
            df[idx] += 1;
        }
        // Pairwise co-occurrence (ordered (min,max) keys).
        for a in 0..present.len() {
            for b in (a + 1)..present.len() {
                let (lo, hi) = if present[a] < present[b] {
                    (present[a], present[b])
                } else {
                    (present[b], present[a])
                };
                *codf.entry((lo, hi)).or_insert(0) += 1;
            }
        }
    }

    // Per-topic NPMI + UMass over the topic's own top terms.
    let mut npmi_sum = 0.0f64;
    let mut npmi_topics = 0usize;
    let mut umass_sum = 0.0f64;
    let mut umass_topics = 0usize;

    for t in topics {
        // Indices of this topic's top terms that are in the vocabulary.
        let idxs: Vec<usize> = t
            .keywords
            .iter()
            .take(top_n)
            .filter_map(|w| vocab.get(w).copied())
            .collect();
        if idxs.len() < 2 {
            continue;
        }

        // NPMI: mean over unordered pairs.
        let mut npmi_acc = 0.0f64;
        let mut npmi_pairs = 0usize;
        for a in 0..idxs.len() {
            for b in (a + 1)..idxs.len() {
                let (ia, ib) = (idxs[a], idxs[b]);
                if df[ia] == 0 || df[ib] == 0 {
                    continue;
                }
                let (lo, hi) = if ia < ib { (ia, ib) } else { (ib, ia) };
                let co = *codf.get(&(lo, hi)).unwrap_or(&0);
                npmi_acc += npmi(df[ia], df[ib], co, n_docs);
                npmi_pairs += 1;
            }
        }
        if npmi_pairs > 0 {
            npmi_sum += npmi_acc / npmi_pairs as f64;
            npmi_topics += 1;
        }

        // UMass: words ordered by descending df, sum_{i<j} ln((co+1)/df[w_j]).
        let mut ordered = idxs.clone();
        ordered.sort_by(|&x, &y| df[y].cmp(&df[x]));
        let mut umass_acc = 0.0f64;
        let mut umass_pairs = 0usize;
        for a in 0..ordered.len() {
            for b in (a + 1)..ordered.len() {
                let (hi_freq, lo_freq) = (ordered[a], ordered[b]);
                if df[lo_freq] == 0 {
                    continue;
                }
                let (lo, hi) = if hi_freq < lo_freq {
                    (hi_freq, lo_freq)
                } else {
                    (lo_freq, hi_freq)
                };
                let co = *codf.get(&(lo, hi)).unwrap_or(&0) as f64;
                umass_acc += ((co + 1.0) / df[lo_freq] as f64).ln();
                umass_pairs += 1;
            }
        }
        if umass_pairs > 0 {
            umass_sum += umass_acc / umass_pairs as f64;
            umass_topics += 1;
        }
    }

    let npmi_mean = if npmi_topics > 0 {
        npmi_sum / npmi_topics as f64
    } else {
        f64::NAN
    };
    let umass_mean = if umass_topics > 0 {
        umass_sum / umass_topics as f64
    } else {
        f64::NAN
    };
    (npmi_mean, umass_mean)
}

/// NPMI for one term pair from document/co-document counts.
///
/// `NPMI = ln(P(a,b)/(P(a)P(b))) / -ln(P(a,b))`, clamped to [-1, 1]. When the
/// pair never co-occurs (`co == 0`) the limit is `-1`.
fn npmi(df_a: u32, df_b: u32, co: u32, n_docs: f64) -> f64 {
    if co == 0 {
        return -1.0;
    }
    let p_a = df_a as f64 / n_docs;
    let p_b = df_b as f64 / n_docs;
    let p_ab = co as f64 / n_docs;
    let pmi = (p_ab / (p_a * p_b)).ln();
    let denom = -p_ab.ln();
    if denom <= 0.0 {
        // p_ab == 1 ⇒ the two terms always co-occur in every doc ⇒ perfectly
        // coherent.
        return 1.0;
    }
    (pmi / denom).clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::topic_clustering::TopicResult;

    fn topic(label: &str, keywords: &[&str], chunk_ids: &[i64]) -> TopicResult {
        TopicResult {
            cluster_index: 0,
            label: label.to_string(),
            keywords: keywords.iter().map(|s| s.to_string()).collect(),
            keyword_scores: Vec::new(),
            chunk_ids: chunk_ids.to_vec(),
            memberships: vec![1.0; chunk_ids.len()],
            file_ids: Vec::new(),
            project_names: Vec::new(),
            avg_internal_similarity: 0.0,
            representative_chunk_id: 0,
            representative_snippet: String::new(),
            top_files: Vec::new(),
            centroid: Vec::new(),
            parent_topic_ids: Vec::new(),
        }
    }

    #[test]
    fn distinct_label_ratio_collapsed_vs_healthy() {
        let collapsed: Vec<TopicResult> = (0..10)
            .map(|i| topic("the / and", &["the", "and"], &[i]))
            .collect();
        assert!(distinct_label_ratio(&collapsed) < 0.2);

        let healthy: Vec<TopicResult> = (0..10)
            .map(|i| {
                let l = format!("label_{i}");
                topic(&l, &["x"], &[i])
            })
            .collect();
        assert_eq!(distinct_label_ratio(&healthy), 1.0);
    }

    #[test]
    fn topic_diversity_repeated_terms_low() {
        let repeated: Vec<TopicResult> = (0..5).map(|i| topic("t", &["a", "b"], &[i])).collect();
        // 10 total terms, 2 unique → 0.2
        assert!((topic_diversity(&repeated, 10) - 0.2).abs() < 1e-9);

        let diverse = vec![
            topic("t0", &["a", "b"], &[0]),
            topic("t1", &["c", "d"], &[1]),
        ];
        assert_eq!(topic_diversity(&diverse, 10), 1.0);
    }

    #[test]
    fn assignment_spread_smearing_detected() {
        // 3 docs each in all 4 topics → topics_per_doc = 12/3 = 4.
        let smeared: Vec<TopicResult> = (0..4)
            .map(|t| topic(&format!("t{t}"), &["x"], &[1, 2, 3]))
            .collect();
        let (per_doc, _share) = assignment_spread(&smeared);
        assert!((per_doc - 4.0).abs() < 1e-9, "per_doc={per_doc}");

        // sparse: each doc in exactly one topic → per_doc = 1.
        let sparse = vec![
            topic("t0", &["x"], &[1]),
            topic("t1", &["x"], &[2]),
            topic("t2", &["x"], &[3]),
        ];
        let (per_doc, share) = assignment_spread(&sparse);
        assert!((per_doc - 1.0).abs() < 1e-9);
        assert!((share - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn max_topic_share_mega_bucket() {
        let topics = vec![
            topic("big", &["x"], &(0..90).collect::<Vec<_>>()),
            topic("small", &["y"], &(90..100).collect::<Vec<_>>()),
        ];
        let (_per_doc, share) = assignment_spread(&topics);
        assert!((share - 0.9).abs() < 1e-9, "share={share}");
    }

    #[test]
    fn npmi_always_cooccur_is_one_never_is_minus_one() {
        let n = 100.0;
        // a,b both appear in all 100 docs and co-occur in all → ~1.
        assert!(npmi(100, 100, 100, n) > 0.99);
        // never co-occur → -1.
        assert_eq!(npmi(50, 50, 0, n), -1.0);
        // independent: a in 50, b in 50, co in 25 (= 0.5*0.5*100) → NPMI ≈ 0.
        assert!(npmi(50, 50, 25, n).abs() < 0.05);
    }

    #[test]
    fn coherence_prefers_cooccurring_terms() {
        // Corpus where "alpha"+"beta" always co-occur (coherent topic) and
        // "gamma"+"delta" never do (incoherent topic).
        let docs: Vec<&str> = vec![
            "alpha beta alpha beta",
            "alpha beta gamma",
            "alpha beta delta",
            "alpha beta",
        ];
        let coherent = topic("c", &["alpha", "beta"], &[0]);
        let incoherent = topic("i", &["gamma", "delta"], &[1]);
        let (npmi_c, _) = coherence(&docs, std::slice::from_ref(&coherent), 10);
        let (npmi_i, _) = coherence(&docs, std::slice::from_ref(&incoherent), 10);
        assert!(
            npmi_c > npmi_i,
            "coherent {npmi_c} should beat incoherent {npmi_i}"
        );
    }

    #[test]
    fn gate_flags_collapsed_model() {
        // Simulate the live collapse: K=200, uniform memberships, 1 label.
        let topics: Vec<TopicResult> = (0..200)
            .map(|i| topic("the / and / dylon", &["the", "and"], &[i]))
            .collect();
        let mut m = TopicMetrics::from_topics(200, &topics);
        m.mean_max_membership = 0.005; // 1/200
        let th = DegeneracyThresholds::default();
        let reason = m.degeneracy_reason(&th);
        assert!(reason.is_some(), "collapsed model must be flagged");
    }

    #[test]
    fn gate_passes_healthy_model() {
        let topics: Vec<TopicResult> = (0..30)
            .map(|i| {
                let l = format!("topic {i} kw");
                topic(&l, &[&format!("kw{i}"), "shared"], &[i, i + 100])
            })
            .collect();
        let mut m = TopicMetrics::from_topics(30, &topics);
        m.mean_max_membership = 0.5;
        m.fuzzy_silhouette = 0.2;
        let th = DegeneracyThresholds::default();
        assert!(
            m.degeneracy_reason(&th).is_none(),
            "healthy model flagged: {:?}",
            m.degeneracy_reason(&th)
        );
    }
}
