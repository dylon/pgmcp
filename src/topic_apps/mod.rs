//! Shared corpus topic-clustering for the new topic-model applications (ADR-029,
//! item 14). `cluster_corpus` clusters ANY embedded corpus (work-items, commit
//! messages, prompts, …) into labeled themes via the canonical FCM loop
//! (`crate::fcm::run_seeded`, seeded for determinism, CPU backend to avoid GPU
//! contention at tool-call time), then labels each cluster by its top
//! distinguishing terms. This is the engine behind `work_item_topics`,
//! `commit_topics`, and `prompt_topics`.

#![allow(dead_code)]

use ndarray::Array2;

use crate::fcm::{self, BackendChoice};

/// One discovered theme over a corpus.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CorpusCluster {
    pub cluster_index: usize,
    pub label: String,
    pub size: usize,
    /// Up to 20 representative members: (id, short text).
    pub members: Vec<(i64, String)>,
}

/// Cross-language stoplist for cluster labeling (prose + identifiers).
const STOP: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "have", "not", "are", "was", "but", "all",
    "can", "you", "use", "add", "fix", "via", "per", "out", "get", "set", "new", "now", "any",
    "its", "has", "had", "let", "fn", "pub", "impl", "self", "into", "when", "then", "else",
    "should", "would", "could", "will", "must", "make", "made", "used", "using", "more", "than",
    "they", "them", "their", "what", "which", "who", "how", "why", "where", "does", "did", "done",
];

/// Cluster `rows` = (id, text, embedding) into ≤`k_req` themes (k clamped to
/// `[1, n]`). Deterministic (seed 42, m=1.1, CPU). Empty input → empty output.
pub fn cluster_corpus(rows: &[(i64, String, Vec<f32>)], k_req: usize) -> Vec<CorpusCluster> {
    let n = rows.len();
    if n == 0 {
        return Vec::new();
    }
    let d = rows[0].2.len();
    if d == 0 {
        return Vec::new();
    }
    let k = k_req.clamp(1, n);

    let mut data = Array2::<f32>::zeros((n, d));
    for (i, (_, _, emb)) in rows.iter().enumerate() {
        if emb.len() == d {
            for (j, &v) in emb.iter().enumerate() {
                data[[i, j]] = v;
            }
        }
    }

    let mut backend = match fcm::make_backend(data.clone(), k, BackendChoice::Cpu) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "corpus clustering: backend construction failed");
            return Vec::new();
        }
    };
    let result = match fcm::run_seeded(
        &mut *backend,
        data.view(),
        k,
        1.1,
        50,
        1e-4,
        None,
        None,
        Some(42),
    ) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "corpus clustering: FCM run failed");
            return Vec::new();
        }
    };

    // Hard-assign each item to its argmax-membership cluster.
    let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); k];
    for (i, bucket_of) in (0..n).map(|i| (i, argmax_row(&result.membership, i))) {
        buckets[bucket_of].push(i);
    }

    let mut out = Vec::new();
    for (ci, idxs) in buckets.iter().enumerate() {
        if idxs.is_empty() {
            continue;
        }
        let texts: Vec<&str> = idxs.iter().map(|&i| rows[i].1.as_str()).collect();
        let label = top_terms(&texts, 4);
        let members: Vec<(i64, String)> = idxs
            .iter()
            .take(20)
            .map(|&i| (rows[i].0, truncate(&rows[i].1, 120)))
            .collect();
        out.push(CorpusCluster {
            cluster_index: ci,
            label,
            size: idxs.len(),
            members,
        });
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.size));
    out
}

fn argmax_row(membership: &Array2<f32>, i: usize) -> usize {
    let row = membership.row(i);
    let mut best = 0usize;
    let mut best_v = f32::MIN;
    for (c, &v) in row.iter().enumerate() {
        if v > best_v {
            best_v = v;
            best = c;
        }
    }
    best
}

/// Top-`n` distinguishing terms across the cluster's member texts.
fn top_terms(texts: &[&str], n: usize) -> String {
    use std::collections::HashMap;
    let mut freq: HashMap<String, usize> = HashMap::new();
    for t in texts {
        for w in t.split(|c: char| !c.is_alphanumeric()) {
            let w = w.to_lowercase();
            if w.len() < 3 || STOP.contains(&w.as_str()) {
                continue;
            }
            *freq.entry(w).or_default() += 1;
        }
    }
    let mut v: Vec<(String, usize)> = freq.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let terms: Vec<String> = v.into_iter().take(n).map(|(w, _)| w).collect();
    if terms.is_empty() {
        "(unlabeled)".to_string()
    } else {
        terms.join(", ")
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two well-separated embedding clusters → two themes with sensible labels.
    #[test]
    fn clusters_separable_corpus() {
        let mut rows = Vec::new();
        // Cluster A: embeddings near [1,0,...]; text about "authentication login".
        for i in 0..6 {
            let mut e = vec![0.0f32; 8];
            e[0] = 1.0;
            e[1] = 0.05 * i as f32;
            rows.push((i as i64, "authentication login token".to_string(), e));
        }
        // Cluster B: embeddings near [0,1,...]; text about "database query".
        for i in 6..12 {
            let mut e = vec![0.0f32; 8];
            e[2] = 1.0;
            e[3] = 0.05 * i as f32;
            rows.push((i as i64, "database query index".to_string(), e));
        }
        let clusters = cluster_corpus(&rows, 2);
        assert_eq!(clusters.len(), 2, "two separable groups → two clusters");
        assert_eq!(clusters.iter().map(|c| c.size).sum::<usize>(), 12);
        // Labels come from member text terms.
        let labels: String = clusters.iter().map(|c| c.label.clone()).collect();
        assert!(
            labels.contains("authentication") || labels.contains("database"),
            "labels reflect member terms: {labels}"
        );
    }

    #[test]
    fn empty_corpus_is_empty() {
        assert!(cluster_corpus(&[], 5).is_empty());
    }

    #[test]
    fn top_terms_skips_stopwords() {
        let t = top_terms(&["the database query", "the database index"], 3);
        assert!(t.contains("database"), "{t}");
        assert!(!t.contains("the"), "stopword leaked: {t}");
    }
}
