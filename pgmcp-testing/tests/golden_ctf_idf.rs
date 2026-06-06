//! Golden-file tests for `pgmcp::cron::topic_clustering::compute_ctf_idf`.
//!
//! The golden envelope stores `f64` keyword scores; we compare them
//! element-wise with a 1e-10 tolerance. The word lists must match
//! exactly (set equality with order preserved — c-TF-IDF sorts
//! descending by score) so we treat "different word at index i" as an
//! infinite error.

use ndarray::Array2;
use pgmcp::cron::topic_clustering::{self, TopicKeyword};
use pgmcp_testing::golden::assert_match_epsilon;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CtfIdfInput {
    contents: Vec<String>,
    membership: Array2<f32>,
    top_k: usize,
}

fn run(input: &CtfIdfInput) -> Vec<Vec<TopicKeyword>> {
    let refs: Vec<&str> = input.contents.iter().map(|s| s.as_str()).collect();
    topic_clustering::compute_ctf_idf(&refs, &input.membership, input.top_k)
}

/// Per-element max error. Returns `f64::INFINITY` if word identity or
/// per-topic length disagrees — those failures are non-numeric and
/// should never be smoothed by a tolerance.
fn max_keyword_error(expected: &[Vec<TopicKeyword>], actual: &[Vec<TopicKeyword>]) -> f64 {
    if expected.len() != actual.len() {
        return f64::INFINITY;
    }
    let mut worst: f64 = 0.0;
    for (e_topic, a_topic) in expected.iter().zip(actual.iter()) {
        if e_topic.len() != a_topic.len() {
            return f64::INFINITY;
        }
        for (e, a) in e_topic.iter().zip(a_topic.iter()) {
            if e.word != a.word {
                return f64::INFINITY;
            }
            let d = (e.score - a.score).abs();
            if d > worst {
                worst = d;
            }
        }
    }
    worst
}

#[test]
fn three_topics_ten_chunks_matches_golden() {
    assert_match_epsilon::<CtfIdfInput, Vec<Vec<TopicKeyword>>>(
        "ctf_idf/three_topics_ten_chunks",
        run,
        |expected, actual| max_keyword_error(expected, actual),
    );
}
