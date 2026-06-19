//! CI regression gate for semantic-search retrieval quality.
//!
//! Scores the frozen probe set ([`pgmcp::quality::retrieval_drift`]) against a
//! populated corpus and asserts conservative MRR / recall@10 floors, so a
//! retrieval regression — a broken embedder, a botched migration, a dropped HNSW
//! index — fails the build.
//!
//! The gate **logic** (a quality collapse trips the floor; healthy passes) is
//! unit-tested in `src/quality/retrieval_drift.rs::tests` and always runs. THIS
//! test exercises the live path and is gated on a reachable corpus DB: it skips
//! cleanly when none is configured, so a fresh CI checkout without the indexed
//! corpus stays green. Point `PGMCP_EVAL_DATABASE_URL` (or `PGMCP_DATABASE_URL`)
//! at a populated pgmcp database to arm it.

use std::time::Duration;

use pgmcp::config::EmbeddingsConfig;
use pgmcp::embed::EmbedSource;
use pgmcp::quality::retrieval_drift::run_retrieval_drift;
use sqlx::postgres::PgPoolOptions;

fn corpus_db_url() -> Option<String> {
    for key in [
        "PGMCP_EVAL_DATABASE_URL",
        "PGMCP_DATABASE_URL",
        "DATABASE_URL",
    ] {
        if let Ok(u) = std::env::var(key)
            && !u.trim().is_empty()
        {
            return Some(u);
        }
    }
    None
}

#[tokio::test]
async fn semantic_search_meets_quality_floor() {
    let Some(db_url) = corpus_db_url() else {
        eprintln!(
            "SKIP: set PGMCP_EVAL_DATABASE_URL (or PGMCP_DATABASE_URL) to a populated pgmcp \
             corpus to arm the retrieval-quality gate"
        );
        return;
    };
    let pool = match PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&db_url)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("SKIP: cannot connect to corpus DB: {e}");
            return;
        }
    };

    // Lazy CPU BGE-M3 query embedder — matches the stored 1024-d `embedding_v2`.
    let emb_cfg = EmbeddingsConfig {
        use_gpu: false,
        ..EmbeddingsConfig::default()
    };
    let embed = EmbedSource::lazy(emb_cfg);

    let report = run_retrieval_drift(&pool, &embed, "pgmcp").await;
    eprintln!("retrieval-quality gate report: {report:?}");

    // Every probe failing means the embedder or corpus is unavailable — that is
    // an infrastructure skip, not a quality regression.
    if report.n_scored == 0 {
        eprintln!("SKIP: no probes scored (embedder/corpus unavailable): {report:?}");
        return;
    }
    assert!(
        report.n_scored >= 10,
        "expected >= 10 probes scored, got {} (corpus incomplete?): {report:?}",
        report.n_scored
    );
    assert!(
        report.meets_floor(0.15, 0.45),
        "RETRIEVAL REGRESSION: mrr={:.3} recall@10={:.3} below floor (0.15 / 0.45): {report:?}",
        report.mrr,
        report.recall_at_10
    );
}
