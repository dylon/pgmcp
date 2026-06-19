//! Retrieval-quality drift detection: a small **frozen** known-item probe set
//! scored with the rank metrics in [`crate::quality::retrieval_metrics`].
//!
//! This is the lightweight, always-available complement to the full evaluation
//! campaign (`pgmcp-testing/src/bin/eval_retrieval.rs`, ~50 hand-authored
//! queries + leakage-controlled strata). It is shared by two callers:
//!
//! - the **`retrieval-eval` cron** ([`crate::cron::retrieval_eval`]) — periodic
//!   on-prem regression detection, persisting the report into `pgmcp_metadata`;
//! - the **CI regression gate** (`pgmcp-testing/tests/eval_semantic_quality.rs`)
//!   — asserts conservative MRR / recall floors so a retrieval regression (a
//!   broken embedder, a botched migration, an index drop) fails the build.
//!
//! Keeping the probe set + scoring here (the main crate) lets both callers share
//! exactly one source of truth; the test crate cannot reach into a binary.
//!
//! The probes deliberately use *intent* phrasing (not the gold file's literal
//! identifier) so they exercise semantic recall, mirroring the eval harness's
//! known-item discipline. Floors are set well below the measured baseline
//! (known-item MRR ≈ 0.30, recall@10 ≈ 0.74) because the live corpus is
//! non-stationary (±0.02 drift as `src/` is edited): the gate must catch a
//! *collapse*, not normal churn.

use sqlx::PgPool;

use crate::db::queries;
use crate::embed::EmbedSource;
use crate::quality::retrieval_metrics::{
    GoldItem, MatchGranularity, RankedHit, compute_query_metrics,
};

/// One frozen `(query, gold-file)` probe over the `pgmcp` project.
pub struct DriftProbe {
    pub query: &'static str,
    pub gold_path: &'static str,
}

/// The frozen probe set — a stable subset of the eval harness's known-item
/// queries whose gold files are long-lived. Twelve probes keep the cron + CI
/// gate fast (one embedding + one HNSW query each) while sampling retrieval,
/// indexing, the tracker, security, and graph subsystems.
pub const DRIFT_PROBES: &[DriftProbe] = &[
    DriftProbe {
        query: "where is the approximate nearest-neighbor index for embeddings created",
        gold_path: "src/db/migrations.rs",
    },
    DriftProbe {
        query: "how are search results ranked by vector distance to the query",
        gold_path: "src/db/queries/search.rs",
    },
    DriftProbe {
        query: "how is the text embedding model loaded onto the GPU",
        gold_path: "src/embed/model.rs",
    },
    DriftProbe {
        query: "where is file content split into overlapping windows before embedding",
        gold_path: "src/indexer/chunker.rs",
    },
    DriftProbe {
        query: "combine keyword and vector result lists into one ranked list",
        gold_path: "src/mcp/tools/tool_hybrid_search.rs",
    },
    DriftProbe {
        query: "rules preventing an agent from marking its own work as verified",
        gold_path: "src/tracker/transition.rs",
    },
    DriftProbe {
        query: "a cross-encoder that re-scores candidate passages for the memory server",
        gold_path: "src/reranker/bge_v2_m3.rs",
    },
    DriftProbe {
        query: "limiting how many embedding models are resident on the GPU at once",
        gold_path: "src/embed/admission.rs",
    },
    DriftProbe {
        query: "finding hardcoded credentials and API keys in source files",
        gold_path: "src/mcp/tools/tool_secret_detection.rs",
    },
    DriftProbe {
        query: "grouping the import graph into communities of related modules",
        gold_path: "src/mcp/tools/tool_community_detection.rs",
    },
    DriftProbe {
        query: "watching free disk space and inodes to avoid filling the volume",
        gold_path: "src/health/watchdog.rs",
    },
    DriftProbe {
        query: "deferring HTTP posts when the database is unavailable and replaying later",
        gold_path: "src/health/outbox.rs",
    },
];

/// Aggregate retrieval-quality over the frozen probe set, at file granularity.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RetrievalDriftReport {
    pub project: String,
    pub n_probes: usize,
    /// Probes that produced a ranking (embed + search succeeded).
    pub n_scored: usize,
    pub mrr: f64,
    pub recall_at_10: f64,
    pub success_at_1: f64,
    /// Probes that failed to embed or search (infra errors, not low quality).
    pub failures: usize,
}

impl RetrievalDriftReport {
    /// The pass predicate for the cron warning + CI gate. Requires at least one
    /// scored probe AND both means at or above the floors. A `collapse` (broken
    /// embedder / dropped index) drives `mrr`/`recall_at_10` to ~0, tripping it.
    pub fn meets_floor(&self, min_mrr: f64, min_recall_at_10: f64) -> bool {
        self.n_scored > 0 && self.mrr >= min_mrr && self.recall_at_10 >= min_recall_at_10
    }
}

/// Run every probe through `semantic_search` and aggregate the rank metrics.
/// Pure with respect to the DB + embedder (no writes); infra failures are
/// counted, not propagated, so a single transient error doesn't abort the scan.
pub async fn run_retrieval_drift(
    pool: &PgPool,
    embed: &EmbedSource,
    project: &str,
) -> RetrievalDriftReport {
    let mut mrr: Vec<f64> = Vec::with_capacity(DRIFT_PROBES.len());
    let mut r10: Vec<f64> = Vec::with_capacity(DRIFT_PROBES.len());
    let mut s1: Vec<f64> = Vec::with_capacity(DRIFT_PROBES.len());
    let mut failures = 0usize;

    for probe in DRIFT_PROBES {
        let emb = match embed.embed_query(probe.query).await {
            Ok(e) => e,
            Err(_) => {
                failures += 1;
                continue;
            }
        };
        let results =
            match queries::semantic_search(pool, &emb, 10, None, Some(project), 100, false).await {
                Ok(r) => r,
                Err(_) => {
                    failures += 1;
                    continue;
                }
            };
        let hits: Vec<RankedHit> = results
            .iter()
            .map(|r| RankedHit::path_only(&r.relative_path))
            .collect();
        let gold = vec![GoldItem {
            path: probe.gold_path.to_string(),
            start_line: None,
            end_line: None,
            relevance: 1.0,
        }];
        let m = compute_query_metrics(&hits, &gold, MatchGranularity::File, &[1, 10]);
        mrr.push(m.reciprocal_rank);
        r10.push(m.recall_at(10));
        s1.push(
            m.at_k
                .iter()
                .find(|x| x.k == 1)
                .map(|x| x.success)
                .unwrap_or(0.0),
        );
    }

    let mean = |v: &[f64]| {
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };
    RetrievalDriftReport {
        project: project.to_string(),
        n_probes: DRIFT_PROBES.len(),
        n_scored: mrr.len(),
        mrr: mean(&mrr),
        recall_at_10: mean(&r10),
        success_at_1: mean(&s1),
        failures,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_set_is_nonempty_and_well_formed() {
        assert!(DRIFT_PROBES.len() >= 10);
        for p in DRIFT_PROBES {
            assert!(!p.query.trim().is_empty());
            assert!(p.gold_path.starts_with("src/") || p.gold_path.starts_with("scripts/"));
        }
    }

    #[test]
    fn meets_floor_catches_collapse_and_passes_healthy() {
        // Healthy: above floors → passes.
        let healthy = RetrievalDriftReport {
            project: "pgmcp".into(),
            n_probes: 12,
            n_scored: 12,
            mrr: 0.30,
            recall_at_10: 0.74,
            success_at_1: 0.14,
            failures: 0,
        };
        assert!(
            healthy.meets_floor(0.15, 0.45),
            "healthy must pass the gate"
        );
        // Collapse (broken embedder / dropped index → ~0) → fails.
        let collapsed = RetrievalDriftReport {
            mrr: 0.0,
            recall_at_10: 0.0,
            ..healthy.clone()
        };
        assert!(
            !collapsed.meets_floor(0.15, 0.45),
            "a retrieval collapse must trip the gate"
        );
        // Zero scored (all infra failures) → fails regardless of means.
        let no_data = RetrievalDriftReport {
            n_scored: 0,
            ..healthy.clone()
        };
        assert!(!no_data.meets_floor(0.0, 0.0), "no scored probes must fail");
    }
}
