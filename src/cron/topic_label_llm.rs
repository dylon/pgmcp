//! LLM topic labeling (Phase 4) — turn each topic's c-TF-IDF keywords +
//! representative snippet into a short human-readable label via the local
//! qwen3-4b model.
//!
//! Design choices:
//! - **Deterministic fallback always present.** The c-TF-IDF keyword label is
//!   computed first (in the clustering paths) and kept as
//!   `TopicResult.keywords`; the LLM only *replaces* `TopicResult.label`. If the
//!   model is unavailable or returns junk, the keyword label stands. No topic is
//!   ever left empty or `topic_N` because of an LLM failure.
//! - **MMR keyword diversity.** Before prompting, the keyword list is
//!   diversity-reranked ([`mmr_rerank`]) so near-duplicate stems
//!   (`tokenize`/`tokenizeHTTP`/`token`) don't crowd the prompt — the BERTopic
//!   representation-tuning step.
//! - **Cache by content.** Labels are memoized by a hash of (keywords, snippet)
//!   so re-running a scan over unchanged topics costs no inference.
//!
//! The completion is injected as a closure so this module is unit-testable with
//! a stub and decoupled from model construction (which is heavy and happens once
//! per scan in the caller).

use std::collections::HashMap;

use crate::cron::topic_clustering::TopicResult;

/// Build the labeling prompt from a topic's top keywords and a representative
/// code/text snippet. Kept short + instruction-shaped for a 4B instruct model.
pub fn build_label_prompt(keywords: &[String], snippet: &str) -> String {
    let kw = keywords.join(", ");
    // Bound the snippet so the prompt stays small and fast.
    let snip: String = snippet.chars().take(600).collect();
    format!(
        "You are labeling a cluster of related source-code/document chunks.\n\
         Top keywords: {kw}\n\
         Representative excerpt:\n\
         ---\n{snip}\n---\n\
         Give a concise topic label of 3 to 7 words naming what this cluster is about. \
         Reply with ONLY the label text, no quotes, no punctuation at the end, no explanation."
    )
}

/// Normalize an LLM response into a clean 3–7 word label. Takes the first
/// non-empty line, strips quotes/markdown/trailing punctuation, collapses
/// whitespace, and caps the word count. Returns an empty string if nothing
/// usable remains (caller then keeps the deterministic label).
pub fn parse_label(raw: &str) -> String {
    let line = raw
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("");
    // Strip common wrappers.
    let line = line
        .trim_start_matches(['"', '\'', '`', '*', '#', '-', ' '])
        .trim_end_matches(['"', '\'', '`', '*', '.', ':', ' ']);
    // Drop a leading "Label:" / "Topic:" prefix if the model added one.
    let line = line
        .strip_prefix("Label:")
        .or_else(|| line.strip_prefix("Topic:"))
        .or_else(|| line.strip_prefix("label:"))
        .unwrap_or(line)
        .trim();
    let words: Vec<&str> = line.split_whitespace().take(8).collect();
    words.join(" ")
}

/// Stable hash of a topic's label inputs, for the memoization cache.
fn label_cache_key(keywords: &[String], snippet: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    keywords.hash(&mut h);
    // Only the snippet head matters for the label; hash a bounded prefix so
    // trivial tail differences still hit the cache.
    let head: String = snippet.chars().take(600).collect();
    head.hash(&mut h);
    h.finish()
}

/// Maximal-Marginal-Relevance rerank of keyword strings to drop near-duplicate
/// stems. Similarity here is a cheap character-trigram Jaccard (no embeddings
/// needed for short tokens). Returns at most `top_n` diverse keywords, order
/// preserved by original rank where possible.
pub fn mmr_rerank(keywords: &[String], top_n: usize, lambda: f64) -> Vec<String> {
    if keywords.len() <= 1 {
        return keywords.to_vec();
    }
    let trigrams: Vec<std::collections::HashSet<[u8; 3]>> =
        keywords.iter().map(|k| char_trigrams(k)).collect();
    let mut selected: Vec<usize> = Vec::new();
    let mut remaining: Vec<usize> = (0..keywords.len()).collect();

    // Seed with the top-ranked keyword (index 0 = highest c-TF-IDF score).
    selected.push(remaining.remove(0));
    while selected.len() < top_n && !remaining.is_empty() {
        // Pick the remaining item maximizing λ·rank_score − (1−λ)·max_sim_to_selected.
        // rank_score = 1 - position/len (earlier = higher relevance).
        let n = keywords.len() as f64;
        let mut best_i = 0usize;
        let mut best_score = f64::NEG_INFINITY;
        for (ri, &idx) in remaining.iter().enumerate() {
            let relevance = 1.0 - (idx as f64) / n;
            let max_sim = selected
                .iter()
                .map(|&s| jaccard(&trigrams[idx], &trigrams[s]))
                .fold(0.0, f64::max);
            let score = lambda * relevance - (1.0 - lambda) * max_sim;
            if score > best_score {
                best_score = score;
                best_i = ri;
            }
        }
        selected.push(remaining.remove(best_i));
    }
    selected.into_iter().map(|i| keywords[i].clone()).collect()
}

fn char_trigrams(s: &str) -> std::collections::HashSet<[u8; 3]> {
    let bytes: Vec<u8> = s.bytes().collect();
    let mut set = std::collections::HashSet::new();
    if bytes.len() < 3 {
        if !bytes.is_empty() {
            let mut t = [0u8; 3];
            for (i, b) in bytes.iter().enumerate() {
                t[i] = *b;
            }
            set.insert(t);
        }
        return set;
    }
    for w in bytes.windows(3) {
        set.insert([w[0], w[1], w[2]]);
    }
    set
}

fn jaccard(a: &std::collections::HashSet<[u8; 3]>, b: &std::collections::HashSet<[u8; 3]>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 { 0.0 } else { inter / union }
}

/// Relabel topics in place using the injected `complete` closure (the qwen3
/// `complete(prompt, max_new_tokens)` in production). Diversity-reranks keywords
/// via MMR for the prompt, memoizes by content, and falls back to the existing
/// deterministic label on any failure or empty result.
///
/// Returns the number of labels actually replaced by the LLM.
pub fn relabel_with<F>(
    topics: &mut [TopicResult],
    top_n: usize,
    cache: &mut HashMap<u64, String>,
    mut complete: F,
) -> usize
where
    F: FnMut(&str) -> anyhow::Result<String>,
{
    let mut replaced = 0usize;
    for t in topics.iter_mut() {
        if t.keywords.is_empty() {
            continue; // nothing to label from; keep deterministic fallback
        }
        let diverse = mmr_rerank(&t.keywords, top_n, 0.7);
        let key = label_cache_key(&diverse, &t.representative_snippet);
        if let Some(cached) = cache.get(&key) {
            if !cached.is_empty() {
                t.label = cached.clone();
                replaced += 1;
            }
            continue;
        }
        let prompt = build_label_prompt(&diverse, &t.representative_snippet);
        match complete(&prompt) {
            Ok(raw) => {
                let label = parse_label(&raw);
                cache.insert(key, label.clone());
                if !label.is_empty() {
                    t.label = label;
                    replaced += 1;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "LLM relabel failed; keeping deterministic label");
                // Negative-cache nothing (transient errors should retry next run).
            }
        }
    }
    replaced
}

/// Map a `topic_llm_backend` string to a local qwen3 variant, if any.
/// Mirrors `worklog::narrative::variant_of`.
fn variant_of(backend: &str) -> Option<crate::llm::qwen3::Qwen3Variant> {
    match backend.trim().to_ascii_lowercase().as_str() {
        "qwen3-8b" => Some(crate::llm::qwen3::Qwen3Variant::Eight),
        "qwen3-4b" => Some(crate::llm::qwen3::Qwen3Variant::Four),
        _ => None,
    }
}

/// Blocking: load the qwen3 model once and relabel every topic. Returns the
/// (possibly-relabeled) topics; on any model failure the deterministic labels
/// are preserved. Designed to run under `tokio::task::spawn_blocking` (the qwen3
/// `complete()` is a synchronous, GPU-bound, mutex-serialized call).
pub fn relabel_topics_blocking(
    mut topics: Vec<TopicResult>,
    backend: &str,
    top_n: usize,
) -> Vec<TopicResult> {
    use crate::llm::qwen3::Qwen3LocalExtractor;
    let Some(variant) = variant_of(backend) else {
        tracing::info!(
            backend,
            "topic LLM labeling: backend is not a local qwen3 variant; keeping deterministic labels"
        );
        return topics;
    };
    let model = match Qwen3LocalExtractor::new(variant) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, backend, "topic LLM labeling: qwen3 load failed; deterministic labels");
            return topics;
        }
    };
    let mut cache: HashMap<u64, String> = HashMap::new();
    // 32 new tokens is ample for a 3–7 word label.
    let replaced = relabel_with(&mut topics, top_n, &mut cache, |p| model.complete(p, 32));
    tracing::info!(
        topics = topics.len(),
        replaced,
        "topic LLM labeling complete"
    );
    topics
}

/// Async wrapper: relabel via `spawn_blocking` when `topic_llm_labels` is set,
/// else return `topics` unchanged. Clones the input so a task-join failure
/// preserves the deterministic labels rather than losing the topics.
pub async fn maybe_relabel(
    topics: Vec<TopicResult>,
    config: &crate::config::CronConfig,
) -> Vec<TopicResult> {
    if !config.topic_llm_labels {
        return topics;
    }
    let backend = config.topic_llm_backend.clone();
    let top_n = config.topic_label_top_k;
    let fallback = topics.clone();
    match tokio::task::spawn_blocking(move || relabel_topics_blocking(topics, &backend, top_n))
        .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "topic LLM labeling task join failed; deterministic labels");
            fallback
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topic(label: &str, keywords: &[&str], snippet: &str) -> TopicResult {
        TopicResult {
            cluster_index: 0,
            label: label.to_string(),
            keywords: keywords.iter().map(|s| s.to_string()).collect(),
            keyword_scores: Vec::new(),
            chunk_ids: vec![1],
            memberships: vec![1.0],
            file_ids: vec![1],
            project_names: vec!["p".into()],
            avg_internal_similarity: 0.0,
            representative_chunk_id: 1,
            representative_snippet: snippet.to_string(),
            top_files: Vec::new(),
            centroid: Vec::new(),
            parent_topic_ids: Vec::new(),
        }
    }

    #[test]
    fn parse_label_strips_wrappers_and_caps_words() {
        assert_eq!(
            parse_label("\"Fuzzy topic clustering engine\""),
            "Fuzzy topic clustering engine"
        );
        assert_eq!(
            parse_label("Label: Graph community detection\n(more text)"),
            "Graph community detection"
        );
        assert_eq!(
            parse_label("one two three four five six seven eight nine ten"),
            "one two three four five six seven eight"
        );
        assert_eq!(parse_label("   \n  "), "");
    }

    #[test]
    fn mmr_drops_near_duplicate_stems() {
        let kws: Vec<String> = ["token", "tokens", "tokenize", "cluster", "graph"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = mmr_rerank(&kws, 3, 0.7);
        // Should prefer diverse picks over the token/tokens/tokenize family.
        assert_eq!(out.len(), 3);
        assert!(out.contains(&"token".to_string()));
        assert!(
            out.contains(&"cluster".to_string()) || out.contains(&"graph".to_string()),
            "expected a diverse pick, got {out:?}"
        );
    }

    #[test]
    fn relabel_uses_llm_then_caches_and_falls_back() {
        let mut topics = vec![
            topic("kw a / kw b", &["alpha", "beta"], "fn alpha_beta() {}"),
            topic("", &[], "no keywords here"), // empty keywords → skipped
        ];
        let mut cache = HashMap::new();
        let mut calls = 0;
        let replaced = relabel_with(&mut topics, 5, &mut cache, |_p| {
            calls += 1;
            Ok("Alpha Beta Subsystem".to_string())
        });
        assert_eq!(replaced, 1);
        assert_eq!(topics[0].label, "Alpha Beta Subsystem");
        assert_eq!(topics[1].label, ""); // unchanged (no keywords)
        assert_eq!(calls, 1);

        // Second pass: same content → served from cache, no new calls.
        let mut calls2 = 0;
        relabel_with(&mut topics, 5, &mut cache, |_p| {
            calls2 += 1;
            Ok("SHOULD NOT BE CALLED".to_string())
        });
        assert_eq!(calls2, 0, "cache should prevent re-inference");
        assert_eq!(topics[0].label, "Alpha Beta Subsystem");
    }

    #[test]
    fn relabel_keeps_deterministic_label_on_error() {
        let mut topics = vec![topic("deterministic / label", &["x", "y"], "snippet")];
        let mut cache = HashMap::new();
        let replaced = relabel_with(&mut topics, 5, &mut cache, |_p| {
            Err(anyhow::anyhow!("model down"))
        });
        assert_eq!(replaced, 0);
        assert_eq!(topics[0].label, "deterministic / label");
    }
}
