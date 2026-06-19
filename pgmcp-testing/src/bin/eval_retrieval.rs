//! `eval-retrieval` — the retrieval-quality evaluation campaign for pgmcp's
//! search tools.
//!
//! Measures whether `semantic_search` / `hybrid_search` / `text_search` rank
//! relevant code highly, against two objective ground-truth strategies, with
//! rank-based metrics and paired significance tests. See
//! `docs/evaluation/semantic-search-quality.md` for the methodology.
//!
//! ## What it runs
//!
//! 1. **Setup** — loads the live pgmcp config + DB pool and a BGE-M3 embedder
//!    (CPU/F32 by default to avoid contending with the daemon's GPU workers;
//!    `--gpu` opts into CUDA/BF16).
//! 2. **Headline (strategy A)** — the hand-authored known-item set through all
//!    three modes; per-mode MRR / recall@k / nDCG@10 + Wilcoxon / Cliff's δ /
//!    bootstrap-CI / BH-FDR pairwise comparisons + the pattern-catalog-crowding
//!    diagnostic.
//! 3. **HNSW honesty + ef_search ablation** — HNSW recall vs an exact
//!    brute-force scan (index disabled) at ef_search ∈ {40,100,200}, with
//!    latency.
//! 4. **Truncation stat** — fraction of chunks long enough to risk the
//!    512-token embedding cap (char-based proxy).
//! 5. **Strategy B-realism (M2)** — leak-free token-holdout on the live corpus:
//!    each query is text drawn from *beyond* the 512-token embedding window.
//!    Always runs (needs only the live corpus + tokenizer).
//! 6. **Strategy B (M1/M3, `--m1`)** — leakage-controlled docstring-as-query in a
//!    fresh isolated database (each target re-embedded with its doc removed),
//!    plus the M3 identifier-redacted variant. Needs `PGMCP_TEST_DATABASE_URL`.
//!
//! Results are written as JSON (`--out`) and summarized to stdout. The JSON is
//! the durable artifact the evaluation report and the experiment ledger draw
//! from.
//!
//! Usage:
//! ```text
//! cargo run --release -p pgmcp-testing --bin eval-retrieval -- \
//!     [--gpu] [--limit 20] [--m1] [--m1-targets 80] [--m1-distractors 800] \
//!     [--out target/eval/retrieval_results.json]
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use serde::Serialize;
use sqlx::{Connection, PgConnection, PgPool, Row};
use uuid::Uuid;

use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::daemon_state::{DaemonLifecycle, DaemonPhase};
use pgmcp::db::DbClient;
use pgmcp::db::queries::{self};
use pgmcp::embed::backend::CandleBackend;
use pgmcp::embed::model::Embedder;
use pgmcp::embed::{EmbedSource, EmbeddingBackend};
use pgmcp::mcp::server::McpServer;
use pgmcp::quality::retrieval_metrics::{
    GoldItem, MatchGranularity, QueryMetrics, compute_query_metrics,
};
use pgmcp::reranker::{Reranker, RerankerChoice, make_reranker};
use pgmcp::stats::tracker::StatsTracker;

use pgmcp_testing::eval::corpus;
use pgmcp_testing::eval::judge::JudgeClient;
use pgmcp_testing::eval::query::{
    ConceptualQuery, EvalQuery, GoldTarget, QueryStrategy, conceptual_queries, known_item_queries,
};
use pgmcp_testing::eval::rerank::{colbert_rerank, cross_encoder_rerank};
use pgmcp_testing::eval::runner::{
    GraphMode, SearchMode, fetch_candidates, fetch_semantic_candidates, pattern_crowding_at_k,
    run_graph_mode, run_mode,
};
use pgmcp_testing::eval::stats::{
    AlignedMetric, PairwiseComparison, cohens_kappa_quadratic, compare_all_pairs, mean,
};

const KS: [usize; 4] = [1, 5, 10, 20];
const ALPHA: f64 = 0.05;
const EF_GRID: [i32; 3] = [40, 100, 200];

struct Args {
    gpu: bool,
    limit: i32,
    run_m1: bool,
    m1_targets: usize,
    m1_distractors: usize,
    rerank: bool,
    rerank_fetch_n: i32,
    graph: bool,
    judge: bool,
    out: String,
}

fn parse_args() -> Args {
    let mut a = Args {
        gpu: false,
        limit: 20,
        run_m1: false,
        m1_targets: 80,
        m1_distractors: 800,
        rerank: false,
        rerank_fetch_n: 30,
        graph: false,
        judge: false,
        out: "target/eval/retrieval_results.json".to_string(),
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--gpu" => a.gpu = true,
            "--m1" => a.run_m1 = true,
            "--rerank" => a.rerank = true,
            "--graph" => a.graph = true,
            "--judge" => a.judge = true,
            "--rerank-fetch-n" => {
                i += 1;
                a.rerank_fetch_n = argv
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(a.rerank_fetch_n);
            }
            "--limit" => {
                i += 1;
                a.limit = argv.get(i).and_then(|s| s.parse().ok()).unwrap_or(a.limit);
            }
            "--m1-targets" => {
                i += 1;
                a.m1_targets = argv
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(a.m1_targets);
            }
            "--m1-distractors" => {
                i += 1;
                a.m1_distractors = argv
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(a.m1_distractors);
            }
            "--out" => {
                i += 1;
                if let Some(s) = argv.get(i) {
                    a.out = s.clone();
                }
            }
            other => eprintln!("warn: ignoring unknown arg `{other}`"),
        }
        i += 1;
    }
    a
}

// ---------------------------------------------------------------------------
// Result structs (serialized to JSON)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ModeMeans {
    mode: String,
    n: usize,
    mrr: f64,
    ndcg_at_10: f64,
    success_at_1: f64,
    recall_at: BTreeMap<usize, f64>,
    pattern_crowding_at_5: f64,
}

/// Raw per-query metric values for one metric, aligned across modes by
/// `unit_keys` (query id). Retained for transparency, rank-histograms, and
/// feeding the experiment-ledger `record_measurement` (samples + unit_keys).
#[derive(Serialize)]
struct MetricSamples {
    metric: String,
    unit_keys: Vec<String>,
    by_mode: Vec<(String, Vec<f64>)>,
}

#[derive(Serialize)]
struct StratumResult {
    label: String,
    n_queries: usize,
    n_complete: usize,
    per_mode: Vec<ModeMeans>,
    pairwise: Vec<PairwiseComparison>,
    samples: Vec<MetricSamples>,
}

#[derive(Serialize)]
struct EfPoint {
    ef_search: i32,
    mean_recall_vs_exact: f64,
    mean_latency_ms: f64,
}

/// Provenance + inter-judge agreement summary for the LLM-as-judge stratum.
#[derive(Serialize)]
struct JudgeReport {
    primary_model: String,
    kappa_model: Option<String>,
    n_queries_scored: usize,
    n_candidates_graded: usize,
    /// Count of each 0–3 grade the primary judge assigned (relevance distribution).
    grade_histogram: BTreeMap<u8, usize>,
    /// Pooled candidates judged relevant (grade ≥ 1), summed over queries.
    n_relevant: usize,
    /// Quadratic-weighted Cohen's κ between the two judges over the κ sample.
    kappa_quadratic: Option<f64>,
    kappa_n: usize,
    pool_depth_per_mode: i32,
    score_depth: i32,
}

/// The LLM-judge conceptual stratum: per-mode graded-relevance scores plus the
/// judge provenance/agreement report.
#[derive(Serialize)]
struct JudgeStratum {
    stratum: StratumResult,
    report: JudgeReport,
}

#[derive(Serialize)]
struct CampaignResults {
    note: String,
    embedder: String,
    limit: i32,
    corpus: serde_json::Value,
    known_item: StratumResult,
    hnsw_exact_recall_at_ef100: f64,
    ef_search_ablation: Vec<EfPoint>,
    truncation: serde_json::Value,
    m2_holdout: Option<StratumResult>,
    /// The M2 holdout scored at CHUNK granularity — isolates the residual
    /// per-chunk truncation cost (compare vs `m2_holdout`'s file granularity).
    m2_holdout_chunk: Option<StratumResult>,
    m1: Option<StratumResult>,
    m1_redacted: Option<StratumResult>,
    /// Reranker A/B strata (semantic vs cross-encoder vs ColBERT), one per query
    /// set the rerank pass was run on. Empty unless `--rerank`.
    rerank_strata: Vec<StratumResult>,
    /// Graph-augmented A/B strata (semantic vs code_ppr/path/raptor). Empty
    /// unless `--graph`.
    graph_strata: Vec<StratumResult>,
    /// LLM-as-judge conceptual stratum (graded relevance + κ). `None` unless
    /// `--judge` and at least the primary judge was reachable.
    judge: Option<JudgeStratum>,
}

// ---------------------------------------------------------------------------
// Setup helpers
// ---------------------------------------------------------------------------

fn build_server(pool: PgPool, embed: EmbedSource, config: Arc<ArcSwap<Config>>) -> McpServer {
    let db: Arc<dyn DbClient> = Arc::new(pool);
    let lifecycle = DaemonLifecycle::new();
    lifecycle.transition(DaemonPhase::Ready);
    let ctx = SystemContext::production(
        db,
        embed,
        Arc::new(StatsTracker::new()),
        config,
        Arc::new(pgmcp::mcp::logging::LogBroadcaster::new()),
        Arc::new(pgmcp::mcp::tasks::TaskStore::new()),
        lifecycle,
    );
    McpServer::new(ctx)
}

// ---------------------------------------------------------------------------
// Isolated test database for the M1 leakage-controlled stratum
// ---------------------------------------------------------------------------

/// Parse `PGMCP_TEST_DATABASE_URL` into `(base_url, maintenance_url)` where the
/// base is everything before the trailing `/dbname` and the maintenance URL
/// targets the always-present `postgres` database. Returns `None` if the env
/// var is unset/unparseable — the caller then skips M1 cleanly.
fn parse_test_db_url() -> Option<(String, String)> {
    let url = std::env::var("PGMCP_TEST_DATABASE_URL").ok()?;
    let scheme_end = url.find("://")?;
    let after = &url[scheme_end + 3..];
    let slash = after.find('/')?;
    let base = url[..scheme_end + 3 + slash].to_string();
    let maint = format!("{base}/postgres");
    Some((base, maint))
}

/// Run one M1/M3 variant in a freshly-created, isolated database that is
/// migrated, populated (strip-and-re-embed), searched, and then dropped.
///
/// This deliberately does NOT use `pgmcp_testing::db_harness::TestDatabase`:
/// that harness creates per-test DBs `WITH TEMPLATE <shared>`, which fails while
/// its own pool holds the template open. A plain `CREATE DATABASE` + explicit
/// `run_migrations` sidesteps that and never touches the production corpus.
#[allow(clippy::too_many_arguments)]
async fn run_m1_variant(
    base: &str,
    maint: &str,
    backend: &Arc<dyn EmbeddingBackend>,
    cands: &[corpus::DocstringCandidate],
    distractors: &[String],
    redact: bool,
    label: &str,
    limit: i32,
    config_arc: &Arc<ArcSwap<Config>>,
) -> Result<StratumResult> {
    let dbname = format!("pgmcp_m1eval_{}", Uuid::now_v7().simple());
    {
        let mut conn = PgConnection::connect(maint)
            .await
            .context("connect maintenance db")?;
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{dbname}\"")))
            .execute(&mut conn)
            .await
            .with_context(|| format!("create {dbname}"))?;
    }
    let url = format!("{base}/{dbname}");
    let pool = PgPool::connect(&url).await.context("connect isolated db")?;
    pgmcp::db::migrations::run_migrations(&pool, &pgmcp::config::VectorConfig::default())
        .await
        .context("migrate isolated db")?;

    let queries = corpus::seed_test_corpus(&pool, backend, cands, distractors, redact)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let server = build_server(
        pool.clone(),
        EmbedSource::backend(Arc::clone(backend)),
        Arc::clone(config_arc),
    );
    let result = run_stratum(&server, &queries, limit, MatchGranularity::File, label).await;

    // Teardown: close our pool, then terminate stragglers and drop the DB.
    pool.close().await;
    if let Ok(mut conn) = PgConnection::connect(maint).await {
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{dbname}' AND pid <> pg_backend_pid()"
        )))
        .execute(&mut conn)
        .await;
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS \"{dbname}\""
        )))
        .execute(&mut conn)
        .await;
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Stratum runner — one query set through all three modes
// ---------------------------------------------------------------------------

/// Run `queries` through every mode on `server`, scoring against gold at file
/// granularity, and assemble per-mode means + BH-corrected pairwise tests for
/// the headline metrics (nDCG@10, MRR, recall@10).
async fn run_stratum(
    server: &McpServer,
    queries: &[EvalQuery],
    limit: i32,
    gran: MatchGranularity,
    label: &str,
) -> StratumResult {
    let modes = SearchMode::all();
    // per_query[qid][arm] = QueryMetrics (arm = mode tag here; rerank arms reuse
    // the same shape via aggregate_stratum).
    let mut per_query: HashMap<String, HashMap<String, QueryMetrics>> = HashMap::new();
    let mut crowd: HashMap<String, Vec<f64>> = HashMap::new();

    for q in queries {
        for mode in modes {
            match run_mode(server, mode, &q.query, q.project.as_deref(), limit).await {
                Ok(hits) => {
                    let m = compute_query_metrics(&hits, &q.gold_items(), gran, &KS);
                    per_query
                        .entry(q.id.clone())
                        .or_default()
                        .insert(mode.tag().to_string(), m);
                    // Pattern-crowding only over queries whose gold is NOT itself a
                    // pattern file (else a pattern hit is correct, not crowding).
                    if !q.gold.iter().any(|g| g.path.starts_with("src/patterns/")) {
                        crowd
                            .entry(mode.tag().to_string())
                            .or_default()
                            .push(pattern_crowding_at_k(&hits, 5));
                    }
                }
                Err(e) => eprintln!("warn[{label}]: {} / {} failed: {e}", q.id, mode.tag()),
            }
        }
    }

    let arms: Vec<String> = modes.iter().map(|m| m.tag().to_string()).collect();
    aggregate_stratum(label, queries.len(), &arms, &per_query, &crowd)
}

/// Aggregate per-query metrics (keyed by arm label) into a [`StratumResult`]:
/// the paired-comparison set (queries where every arm produced metrics), aligned
/// per-metric vectors, BH-corrected pairwise tests, per-arm means, and raw
/// samples. Shared by [`run_stratum`] (search modes) and [`run_rerank_stratum`]
/// (rerank arms).
fn aggregate_stratum(
    label: &str,
    n_queries: usize,
    arms: &[String],
    per_query: &HashMap<String, HashMap<String, QueryMetrics>>,
    crowd: &HashMap<String, Vec<f64>>,
) -> StratumResult {
    // Queries where ALL arms produced metrics — the paired-comparison set.
    let mut complete: Vec<String> = per_query
        .iter()
        .filter(|(_, m)| arms.iter().all(|a| m.contains_key(a)))
        .map(|(id, _)| id.clone())
        .collect();
    complete.sort();

    // Aligned per-arm vectors for a metric, ordered by `complete`.
    let aligned = |name: &str, f: &dyn Fn(&QueryMetrics) -> f64| -> AlignedMetric {
        let by_mode = arms
            .iter()
            .map(|a| {
                let v: Vec<f64> = complete.iter().map(|id| f(&per_query[id][a])).collect();
                (a.clone(), v)
            })
            .collect();
        AlignedMetric {
            metric: name.to_string(),
            by_mode,
        }
    };

    let ndcg = aligned("ndcg@10", &|m| m.ndcg_at(10));
    let mrr = aligned("mrr", &|m| m.reciprocal_rank);
    let recall10 = aligned("recall@10", &|m| m.recall_at(10));

    let mut pairwise = Vec::new();
    pairwise.extend(compare_all_pairs(&ndcg, ALPHA));
    pairwise.extend(compare_all_pairs(&mrr, ALPHA));
    pairwise.extend(compare_all_pairs(&recall10, ALPHA));

    // Per-arm means over the complete set.
    let per_mode = arms
        .iter()
        .map(|a| {
            let rr: Vec<f64> = complete
                .iter()
                .map(|id| per_query[id][a].reciprocal_rank)
                .collect();
            let nd: Vec<f64> = complete
                .iter()
                .map(|id| per_query[id][a].ndcg_at(10))
                .collect();
            let s1: Vec<f64> = complete
                .iter()
                .map(|id| {
                    per_query[id][a]
                        .at_k
                        .iter()
                        .find(|x| x.k == 1)
                        .map(|x| x.success)
                        .unwrap_or(0.0)
                })
                .collect();
            let mut recall_at = BTreeMap::new();
            for &k in &KS {
                let rk: Vec<f64> = complete
                    .iter()
                    .map(|id| per_query[id][a].recall_at(k))
                    .collect();
                recall_at.insert(k, mean(&rk));
            }
            ModeMeans {
                mode: a.clone(),
                n: complete.len(),
                mrr: mean(&rr),
                ndcg_at_10: mean(&nd),
                success_at_1: mean(&s1),
                recall_at,
                pattern_crowding_at_5: mean(crowd.get(a).map(|v| v.as_slice()).unwrap_or(&[])),
            }
        })
        .collect();

    let to_samples = |am: &AlignedMetric| MetricSamples {
        metric: am.metric.clone(),
        unit_keys: complete.clone(),
        by_mode: am.by_mode.clone(),
    };
    let samples = vec![to_samples(&ndcg), to_samples(&mrr), to_samples(&recall10)];

    StratumResult {
        label: label.to_string(),
        n_queries,
        n_complete: complete.len(),
        per_mode,
        pairwise,
        samples,
    }
}

/// Reranker A/B: for each query fetch the top-`fetch_n` semantic candidates
/// (with passage content), then score three arms — `semantic` (the top-`limit`
/// baseline), `rerank_xenc` (BGE cross-encoder), `rerank_colbert` (ColBERT
/// MaxSim) — reusing [`aggregate_stratum`]. Arms whose model is absent are
/// skipped. A wider `fetch_n` than `limit` lets a reranker promote a candidate
/// from beyond the baseline top-`limit`, mirroring the `/api/search` pipeline.
async fn run_rerank_stratum(
    server: &McpServer,
    queries: &[EvalQuery],
    reranker: Option<&dyn Reranker>,
    colbert: Option<&Embedder>,
    fetch_n: i32,
    limit: i32,
    label: &str,
) -> StratumResult {
    let mut arms: Vec<String> = vec!["semantic".to_string()];
    if reranker.is_some() {
        arms.push("rerank_xenc".to_string());
    }
    if colbert.is_some() {
        arms.push("rerank_colbert".to_string());
    }
    let mut per_query: HashMap<String, HashMap<String, QueryMetrics>> = HashMap::new();
    let crowd: HashMap<String, Vec<f64>> = HashMap::new(); // crowding not meaningful here

    for q in queries {
        let cands = match fetch_semantic_candidates(server, &q.query, q.project.as_deref(), fetch_n)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("warn[{label}]: {} candidate fetch failed: {e}", q.id);
                continue;
            }
        };
        if cands.is_empty() {
            continue;
        }
        let gold = q.gold_items();
        let entry = per_query.entry(q.id.clone()).or_default();

        // Baseline = semantic top-`limit` (the order semantic_search returns).
        let base: Vec<_> = cands
            .iter()
            .take(limit as usize)
            .map(|c| c.hit.clone())
            .collect();
        entry.insert(
            "semantic".to_string(),
            compute_query_metrics(&base, &gold, MatchGranularity::File, &KS),
        );

        if let Some(rr) = reranker {
            match cross_encoder_rerank(rr, &q.query, &cands, limit as usize) {
                Ok(reordered) => {
                    entry.insert(
                        "rerank_xenc".to_string(),
                        compute_query_metrics(&reordered, &gold, MatchGranularity::File, &KS),
                    );
                }
                Err(e) => eprintln!("warn[{label}]: {} xenc rerank failed: {e}", q.id),
            }
        }
        if let Some(cb) = colbert {
            match colbert_rerank(cb, &q.query, &cands, limit as usize) {
                Ok(reordered) => {
                    entry.insert(
                        "rerank_colbert".to_string(),
                        compute_query_metrics(&reordered, &gold, MatchGranularity::File, &KS),
                    );
                }
                Err(e) => eprintln!("warn[{label}]: {} colbert rerank failed: {e}", q.id),
            }
        }
    }

    aggregate_stratum(label, queries.len(), &arms, &per_query, &crowd)
}

/// Graph-augmented A/B: for each query, score `semantic` (baseline top-`limit`)
/// vs `code_ppr` / `code_path` / `code_raptor`, reusing [`aggregate_stratum`].
/// All modes are scored at file granularity (paths/clusters flattened to files);
/// a mode that errors or returns nothing for a query is simply absent for it
/// (and that query drops from the paired-comparison set if any arm is missing).
async fn run_graph_stratum(
    server: &McpServer,
    queries: &[EvalQuery],
    limit: i32,
    label: &str,
) -> StratumResult {
    let graph_modes = GraphMode::all();
    let mut arms: Vec<String> = vec!["semantic".to_string()];
    arms.extend(graph_modes.iter().map(|m| m.tag().to_string()));
    let mut per_query: HashMap<String, HashMap<String, QueryMetrics>> = HashMap::new();
    let crowd: HashMap<String, Vec<f64>> = HashMap::new();

    for q in queries {
        let gold = q.gold_items();
        match run_mode(
            server,
            SearchMode::Semantic,
            &q.query,
            q.project.as_deref(),
            limit,
        )
        .await
        {
            Ok(hits) => {
                per_query.entry(q.id.clone()).or_default().insert(
                    "semantic".to_string(),
                    compute_query_metrics(&hits, &gold, MatchGranularity::File, &KS),
                );
            }
            Err(e) => eprintln!("warn[{label}]: {} semantic failed: {e}", q.id),
        }
        for gm in graph_modes {
            match run_graph_mode(server, gm, &q.query, q.project.as_deref(), limit).await {
                Ok(hits) => {
                    per_query.entry(q.id.clone()).or_default().insert(
                        gm.tag().to_string(),
                        compute_query_metrics(&hits, &gold, MatchGranularity::File, &KS),
                    );
                }
                Err(e) => eprintln!("warn[{label}]: {} {} failed: {e}", q.id, gm.tag()),
            }
        }
    }
    aggregate_stratum(label, queries.len(), &arms, &per_query, &crowd)
}

/// Fetch a representative chunk's content for a path from the DB — the fallback
/// passage for judge pooling when a mode returned a path without content (e.g.
/// `text_search`). Returns the first chunk's content (file-granularity grading
/// needs one representative passage, not every chunk).
async fn fetch_chunk_content(pool: &PgPool, project: &str, path: &str) -> Option<String> {
    sqlx::query_scalar(
        "SELECT c.content FROM file_chunks c \
         JOIN indexed_files f ON f.id = c.file_id \
         JOIN projects p ON p.id = f.project_id \
         WHERE p.name = $1 AND f.relative_path = $2 AND c.content IS NOT NULL \
         ORDER BY c.start_line LIMIT 1",
    )
    .bind(project)
    .bind(path)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// LLM-as-judge conceptual stratum (Epic 2). For each conceptual query: pool the
/// top-`pool_k` candidates across semantic/hybrid/text (TREC-style pooling),
/// grade each pooled `(query, passage)` 0–3 with the PRIMARY judge to build the
/// graded gold, then score each mode's top-`score_limit` ranking against that
/// gold (reusing [`aggregate_stratum`]). A `kappa_sample` prefix of queries is
/// re-graded by the SECOND judge to report cross-family quadratic Cohen's κ.
/// Grading is point-wise (one candidate per call) so there is no list position
/// bias, and the judge never sees which mode produced a candidate.
#[allow(clippy::too_many_arguments)]
async fn run_judge_stratum(
    server: &McpServer,
    pool: &PgPool,
    conceptual: &[ConceptualQuery],
    primary: &JudgeClient,
    kappa_judge: Option<&JudgeClient>,
    pool_k: i32,
    score_limit: i32,
    kappa_sample: usize,
) -> JudgeStratum {
    let modes = SearchMode::all();
    let arms: Vec<String> = modes.iter().map(|m| m.tag().to_string()).collect();
    let mut per_query: HashMap<String, HashMap<String, QueryMetrics>> = HashMap::new();
    let crowd: HashMap<String, Vec<f64>> = HashMap::new();
    let mut grade_histogram: BTreeMap<u8, usize> = BTreeMap::new();
    let mut n_candidates_graded = 0usize;
    let mut n_relevant = 0usize;
    let mut kappa_a: Vec<u8> = Vec::new();
    let mut kappa_b: Vec<u8> = Vec::new();

    for (qi, cq) in conceptual.iter().enumerate() {
        let project = cq.project.clone().unwrap_or_else(|| "pgmcp".to_string());

        // 1. Pool candidates with content across all modes (union by path).
        let mut order: Vec<String> = Vec::new();
        let mut content_by_path: HashMap<String, String> = HashMap::new();
        for mode in modes {
            match fetch_candidates(server, mode, &cq.query, cq.project.as_deref(), pool_k).await {
                Ok(cands) => {
                    for c in cands {
                        let p = c.hit.path.clone();
                        if !content_by_path.contains_key(&p) {
                            order.push(p.clone());
                        }
                        let slot = content_by_path.entry(p).or_default();
                        if slot.is_empty() && !c.content.trim().is_empty() {
                            *slot = c.content;
                        }
                    }
                }
                Err(e) => eprintln!("warn[judge]: {} {} pool failed: {e}", cq.id, mode.tag()),
            }
        }
        // Fill missing content from the DB (e.g. text-only paths).
        for p in &order {
            let needs_content = content_by_path
                .get(p)
                .map(|s| s.trim().is_empty())
                .unwrap_or(true);
            if needs_content && let Some(content) = fetch_chunk_content(pool, &project, p).await {
                content_by_path.insert(p.clone(), content);
            }
        }

        // 2. Grade each pooled candidate → graded gold.
        let do_kappa = kappa_judge.is_some() && qi < kappa_sample;
        let mut gold: Vec<GoldItem> = Vec::new();
        for p in &order {
            let content = content_by_path.get(p).cloned().unwrap_or_default();
            if content.trim().is_empty() {
                continue; // no passage to grade
            }
            match primary.grade(&cq.query, p, &content).await {
                Ok(g) => {
                    *grade_histogram.entry(g).or_default() += 1;
                    n_candidates_graded += 1;
                    if g >= 1 {
                        n_relevant += 1;
                        gold.push(GoldItem {
                            path: p.clone(),
                            start_line: None,
                            end_line: None,
                            relevance: g as f64,
                        });
                    }
                    if do_kappa && let Some(kj) = kappa_judge {
                        match kj.grade(&cq.query, p, &content).await {
                            Ok(g2) => {
                                kappa_a.push(g);
                                kappa_b.push(g2);
                            }
                            Err(e) => eprintln!("warn[judge]: {} κ grade failed: {e}", cq.id),
                        }
                    }
                }
                Err(e) => eprintln!("warn[judge]: {} grade `{p}` failed: {e}", cq.id),
            }
        }
        if gold.is_empty() {
            eprintln!(
                "warn[judge]: {} — no relevant candidate (all graded 0), skipping",
                cq.id
            );
            continue;
        }

        // 3. Score each mode's ranking against the judge-built graded gold.
        for mode in modes {
            match run_mode(server, mode, &cq.query, cq.project.as_deref(), score_limit).await {
                Ok(hits) => {
                    per_query.entry(cq.id.clone()).or_default().insert(
                        mode.tag().to_string(),
                        compute_query_metrics(&hits, &gold, MatchGranularity::File, &KS),
                    );
                }
                Err(e) => eprintln!("warn[judge]: {} {} score failed: {e}", cq.id, mode.tag()),
            }
        }
    }

    let stratum = aggregate_stratum(
        "judge_conceptual",
        conceptual.len(),
        &arms,
        &per_query,
        &crowd,
    );
    let kappa_quadratic =
        (kappa_a.len() >= 2).then(|| cohens_kappa_quadratic(&kappa_a, &kappa_b, 3));
    let report = JudgeReport {
        primary_model: primary.signature().to_string(),
        kappa_model: kappa_judge.map(|k| k.signature().to_string()),
        n_queries_scored: stratum.n_complete,
        n_candidates_graded,
        grade_histogram,
        n_relevant,
        kappa_quadratic,
        kappa_n: kappa_a.len(),
        pool_depth_per_mode: pool_k,
        score_depth: score_limit,
    };
    JudgeStratum { stratum, report }
}

// ---------------------------------------------------------------------------
// HNSW honesty: exact brute-force scan (index disabled), project-scoped
// ---------------------------------------------------------------------------

/// Exact top-`k` (relative_path, start_line) for `project` via a forced
/// sequential scan — the ground truth HNSW recall is measured against.
async fn brute_force_scoped(
    pool: &PgPool,
    emb: &[f32],
    k: i32,
    project: &str,
) -> Result<Vec<(String, i32)>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL enable_indexscan = off")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL enable_bitmapscan = off")
        .execute(&mut *tx)
        .await?;
    let v = pgvector::Vector::from(emb.to_vec());
    let rows = sqlx::query(
        "SELECT f.relative_path, c.start_line \
         FROM file_chunks c \
         JOIN indexed_files f ON f.id = c.file_id \
         JOIN projects p ON p.id = f.project_id \
         WHERE p.name = $1 AND c.embedding_v2 IS NOT NULL \
         ORDER BY c.embedding_v2 <=> $2 LIMIT $3",
    )
    .bind(project)
    .bind(v)
    .bind(k)
    .fetch_all(&mut *tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let p: String = r.get("relative_path");
            let s: i32 = r.get("start_line");
            (p, s)
        })
        .collect())
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args();

    let config = Config::load(None).context("load pgmcp config")?;
    pgmcp::logging::init_cli_with_config(Some(&config));

    let pool = pgmcp::db::pool::create_pool(&config.database)
        .await
        .context("connect to live pgmcp database")?;

    // Real BGE-M3 embedder. CPU/F32 by default (no GPU contention with the
    // running daemon); --gpu opts into CUDA/BF16 (may OOM if the daemon holds
    // the card — see the report's threats section).
    let mut emb_cfg = config.embeddings.clone();
    emb_cfg.use_gpu = args.gpu;
    let embedder_label = if args.gpu { "gpu-bf16" } else { "cpu-f32" };
    eprintln!("[setup] building BGE-M3 embedder ({embedder_label})…");
    let backend: Arc<dyn EmbeddingBackend> = {
        let cfg = emb_cfg.clone();
        let b = tokio::task::spawn_blocking(move || CandleBackend::new(&cfg))
            .await
            .context("embedder construction panicked")?
            .context("CandleBackend::new failed")?;
        Arc::new(b)
    };

    let config_arc = Arc::new(ArcSwap::from_pointee(config));
    let live_server = build_server(
        pool.clone(),
        EmbedSource::backend(Arc::clone(&backend)),
        Arc::clone(&config_arc),
    );

    // --- Corpus snapshot ---
    let corpus = {
        let g = sqlx::query(
            "SELECT (SELECT count(*) FROM projects) AS projects, \
                    (SELECT count(*) FROM indexed_files) AS files, \
                    (SELECT count(*) FROM file_chunks) AS chunks, \
                    (SELECT count(*) FROM file_chunks WHERE embedding_v2 IS NOT NULL) AS embedded",
        )
        .fetch_one(&pool)
        .await?;
        let pg: i64 = g.get("projects");
        let fi: i64 = g.get("files");
        let ch: i64 = g.get("chunks");
        let em: i64 = g.get("embedded");
        serde_json::json!({ "projects": pg, "files": fi, "chunks": ch, "embedded_chunks": em })
    };
    eprintln!("[corpus] {corpus}");

    // --- Headline: strategy A (known-item) ---
    eprintln!("[A] running known-item stratum…");
    let ki = known_item_queries();
    ki.validate();
    let known_item = run_stratum(
        &live_server,
        &ki.queries,
        args.limit,
        MatchGranularity::File,
        "known_item",
    )
    .await;

    // --- HNSW honesty + ef_search ablation (project-scoped to pgmcp) ---
    eprintln!("[hnsw] measuring HNSW-vs-exact recall + ef_search ablation…");
    let mut ef_recall: BTreeMap<i32, Vec<f64>> = BTreeMap::new();
    let mut ef_latency: BTreeMap<i32, Vec<f64>> = BTreeMap::new();
    for q in &ki.queries {
        let Some(project) = q.project.as_deref() else {
            continue;
        };
        let emb = match backend.embed_one(&q.query).await {
            Ok(e) => e,
            Err(e) => {
                eprintln!("warn[hnsw]: embed {} failed: {e}", q.id);
                continue;
            }
        };
        let truth: HashSet<(String, i32)> = brute_force_scoped(&pool, &emb, 10, project)
            .await?
            .into_iter()
            .collect();
        for &ef in &EF_GRID {
            let t0 = Instant::now();
            let res = queries::semantic_search(&pool, &emb, 10, None, Some(project), ef, false)
                .await
                .context("ef_search ablation semantic_search")?;
            let dt = t0.elapsed().as_secs_f64() * 1000.0;
            let got: HashSet<(String, i32)> = res
                .iter()
                .map(|r| (r.relative_path.clone(), r.start_line))
                .collect();
            let overlap = got.intersection(&truth).count();
            let denom = truth.len().clamp(1, 10) as f64;
            ef_recall
                .entry(ef)
                .or_default()
                .push(overlap as f64 / denom);
            ef_latency.entry(ef).or_default().push(dt);
        }
    }
    let ef_search_ablation: Vec<EfPoint> = EF_GRID
        .iter()
        .map(|&ef| EfPoint {
            ef_search: ef,
            mean_recall_vs_exact: mean(ef_recall.get(&ef).map(|v| v.as_slice()).unwrap_or(&[])),
            mean_latency_ms: mean(ef_latency.get(&ef).map(|v| v.as_slice()).unwrap_or(&[])),
        })
        .collect();
    let hnsw_exact_recall_at_ef100 = mean(ef_recall.get(&100).map(|v| v.as_slice()).unwrap_or(&[]));

    // --- Truncation stat (approximate, char-based proxy for the 512-token cap) ---
    eprintln!("[trunc] measuring chunk-length distribution…");
    let truncation = {
        let r = sqlx::query(
            "SELECT count(*) AS total, \
                    count(*) FILTER (WHERE length(content) > 2048) AS over_2048, \
                    avg(length(content))::float8 AS avg_len, \
                    max(length(content)) AS max_len \
             FROM file_chunks WHERE content IS NOT NULL",
        )
        .fetch_one(&pool)
        .await?;
        let total: i64 = r.get("total");
        let over: i64 = r.get("over_2048");
        let avg_len: f64 = r.get("avg_len");
        let max_len: i32 = r.get("max_len");
        serde_json::json!({
            "note": "char-based proxy: >2048 chars ≈ >512 BGE-M3 tokens (~4 chars/token); the tail of such chunks is dropped from the embedding",
            "total_chunks": total,
            "over_2048_chars": over,
            "fraction_at_risk": if total > 0 { over as f64 / total as f64 } else { 0.0 },
            "avg_chars": avg_len,
            "max_chars": max_len,
        })
    };

    // --- Strategy B-realism (M2): token-position hold-out on the live corpus ---
    // Leak-free by construction (the query is text beyond the 512-token embedding
    // window, which the stored vector never encoded) AND a direct truncation-cost
    // measurement (semantic lacks the tail; the FTS index has it). Runs on the
    // live corpus — no isolated DB, no re-embedding — so it is always enabled.
    eprintln!("[B/M2] building token-holdout corpus (live, leak-free)…");
    let mut m2_queries_captured: Vec<EvalQuery> = Vec::new();
    let window = if emb_cfg.max_length == 0 {
        512
    } else {
        emb_cfg.max_length.min(512)
    };
    let m2_holdout = match pgmcp::embed::model::bge_m3_model_dir() {
        Ok(model_dir) => match tokenizers::Tokenizer::from_file(model_dir.join("tokenizer.json")) {
            Ok(mut tok) => {
                let _ = tok.with_truncation(None); // see the full token sequence
                let cands = corpus::collect_holdout_candidates(
                    &pool,
                    "pgmcp",
                    &tok,
                    window,
                    args.m1_targets,
                )
                .await?;
                eprintln!(
                    "[B/M2] collected {} holdout candidates (window={window} tokens)",
                    cands.len()
                );
                if cands.is_empty() {
                    None
                } else {
                    let m2_queries: Vec<EvalQuery> = cands
                        .iter()
                        .enumerate()
                        .map(|(i, c)| EvalQuery {
                            id: format!("m2_{i:04}"),
                            strategy: QueryStrategy::DocstringHoldout,
                            query: c.query.clone(),
                            project: Some("pgmcp".to_string()),
                            gold: vec![GoldTarget {
                                path: c.relative_path.clone(),
                                project: "pgmcp".to_string(),
                                start_line: Some(c.start_line),
                                end_line: Some(c.end_line),
                                relevance: 1.0,
                            }],
                            notes: Some(format!(
                                "holdout tail of {}:{}",
                                c.relative_path, c.start_line
                            )),
                        })
                        .collect();
                    m2_queries_captured = m2_queries.clone();
                    Some(
                        run_stratum(
                            &live_server,
                            &m2_queries,
                            args.limit,
                            MatchGranularity::File,
                            "m2_holdout",
                        )
                        .await,
                    )
                }
            }
            Err(e) => {
                eprintln!("[B/M2] SKIPPED: tokenizer load failed: {e}");
                None
            }
        },
        Err(e) => {
            eprintln!("[B/M2] SKIPPED: model dir unavailable: {e}");
            None
        }
    };

    // Chunk-granularity M2 (truncation isolation): the same beyond-window
    // holdout queries scored at CHUNK granularity. Comparing semantic's
    // file-granularity recall (`m2_holdout`) against its chunk-granularity recall
    // here isolates the residual per-chunk truncation cost (threat T8) — the
    // right *file* is retrieved, but the right *chunk* ranks lower because its
    // >512-token embedding dropped the tail. (text_search returns no line spans,
    // so its chunk-gran row is degenerate; the semantic/hybrid rows carry the
    // signal.)
    let m2_holdout_chunk = if m2_queries_captured.is_empty() {
        None
    } else {
        eprintln!("[B/M2-chunk] scoring holdout at chunk granularity…");
        Some(
            run_stratum(
                &live_server,
                &m2_queries_captured,
                args.limit,
                MatchGranularity::Chunk,
                "m2_holdout_chunk",
            )
            .await,
        )
    };

    // --- Strategy B: M1 leakage-controlled docstring (optional) ---
    let (mut m1, mut m1_redacted) = (None, None);
    if args.run_m1 {
        match parse_test_db_url() {
            Some((base, maint)) => {
                eprintln!("[B/M1] building leakage-controlled docstring corpus…");
                let cands =
                    corpus::collect_docstring_candidates(&pool, "pgmcp", args.m1_targets).await?;
                eprintln!("[B/M1] collected {} docstring candidates", cands.len());
                if cands.is_empty() {
                    eprintln!("[B/M1] no candidates — skipping");
                } else {
                    let exclude: HashSet<String> =
                        cands.iter().map(|c| c.relative_path.clone()).collect();
                    let distractors = corpus::sample_distractor_texts(
                        &pool,
                        "pgmcp",
                        &exclude,
                        args.m1_distractors,
                    )
                    .await?;
                    eprintln!(
                        "[B/M1] embedding {} targets + {} distractors on {} (slow step)…",
                        cands.len(),
                        distractors.len(),
                        embedder_label
                    );
                    m1 = Some(
                        run_m1_variant(
                            &base,
                            &maint,
                            &backend,
                            &cands,
                            &distractors,
                            false,
                            "m1",
                            args.limit,
                            &config_arc,
                        )
                        .await?,
                    );

                    eprintln!("[B/M3] identifier-redacted variant…");
                    m1_redacted = Some(
                        run_m1_variant(
                            &base,
                            &maint,
                            &backend,
                            &cands,
                            &distractors,
                            true,
                            "m1_redacted",
                            args.limit,
                            &config_arc,
                        )
                        .await?,
                    );
                }
            }
            None => eprintln!(
                "[B/M1] SKIPPED: set PGMCP_TEST_DATABASE_URL to a CREATEDB-capable connection \
                 (e.g. postgres://postgres@localhost:5432/postgres) to enable."
            ),
        }
    }

    // --- Reranker A/B (--rerank): semantic vs cross-encoder vs ColBERT ---
    // Second-stage rerank lift over plain dense retrieval, on the live strata
    // (known-item + M2 holdout). Models are local; the cross-encoder is
    // GPU-greedy with no use_gpu knob, so run with CUDA_VISIBLE_DEVICES="" to
    // force CPU alongside the daemon.
    let mut rerank_strata: Vec<StratumResult> = Vec::new();
    if args.rerank {
        eprintln!("[rerank] loading BGE cross-encoder + ColBERT embedder…");
        let reranker: Option<Box<dyn Reranker>> =
            match tokio::task::spawn_blocking(|| make_reranker(RerankerChoice::BgeV2M3)).await {
                Ok(Ok(opt)) => opt,
                Ok(Err(e)) => {
                    eprintln!("[rerank] cross-encoder load failed: {e}");
                    None
                }
                Err(e) => {
                    eprintln!("[rerank] cross-encoder load panicked: {e}");
                    None
                }
            };
        let colbert: Option<Embedder> = {
            let mut cfg = emb_cfg.clone();
            cfg.use_gpu = args.gpu; // follow --gpu (free GPU → fast ColBERT; else CPU)
            match tokio::task::spawn_blocking(move || Embedder::new(&cfg)).await {
                Ok(Ok(e)) if e.has_colbert() => Some(e),
                Ok(Ok(_)) => {
                    eprintln!("[rerank] ColBERT head absent — skipping ColBERT arm");
                    None
                }
                Ok(Err(e)) => {
                    eprintln!("[rerank] ColBERT embedder load failed: {e}");
                    None
                }
                Err(e) => {
                    eprintln!("[rerank] ColBERT embedder panicked: {e}");
                    None
                }
            }
        };
        if reranker.is_some() || colbert.is_some() {
            eprintln!("[rerank] known-item A/B…");
            rerank_strata.push(
                run_rerank_stratum(
                    &live_server,
                    &ki.queries,
                    reranker.as_deref(),
                    colbert.as_ref(),
                    args.rerank_fetch_n,
                    args.limit,
                    "rerank_known_item",
                )
                .await,
            );
            if !m2_queries_captured.is_empty() {
                eprintln!("[rerank] M2 holdout A/B…");
                rerank_strata.push(
                    run_rerank_stratum(
                        &live_server,
                        &m2_queries_captured,
                        reranker.as_deref(),
                        colbert.as_ref(),
                        args.rerank_fetch_n,
                        args.limit,
                        "rerank_m2_holdout",
                    )
                    .await,
                );
            }
        } else {
            eprintln!("[rerank] no reranker model available — skipping rerank A/B");
        }
    }

    // --- Graph-augmented modes (--graph): semantic vs code_ppr/path/raptor ---
    // Calls the graph-aware MCP tools in-process; their artifacts
    // (code_graph_edges, code_summary_tree) must be populated.
    let mut graph_strata: Vec<StratumResult> = Vec::new();
    if args.graph {
        eprintln!("[graph] known-item A/B (semantic vs code_ppr/path/raptor)…");
        graph_strata.push(
            run_graph_stratum(&live_server, &ki.queries, args.limit, "graph_known_item").await,
        );
        if !m2_queries_captured.is_empty() {
            eprintln!("[graph] M2 holdout A/B…");
            graph_strata.push(
                run_graph_stratum(
                    &live_server,
                    &m2_queries_captured,
                    args.limit,
                    "graph_m2_holdout",
                )
                .await,
            );
        }
    }

    // --- LLM-as-judge conceptual stratum (--judge): grades run on sparky ---
    // Pools candidates across modes, grades them 0–3 with a local LLM judge
    // (qwen3-32B on sparky's ollama by default), scores each mode against the
    // judge-built graded gold, and cross-checks a sample with a second judge
    // (DeepSeek-V4) for Cohen's κ. Endpoints/models/sizes are overridable via
    // PGMCP_JUDGE_* env vars. Both judges are probed first; the κ judge is
    // best-effort (κ = None if it is unreachable).
    let judge = if args.judge {
        let primary_url = std::env::var("PGMCP_JUDGE_PRIMARY_URL")
            .unwrap_or_else(|_| "http://sparky:11434/v1".to_string());
        let primary_model =
            std::env::var("PGMCP_JUDGE_PRIMARY_MODEL").unwrap_or_else(|_| "qwen3:32b".to_string());
        let kappa_url = std::env::var("PGMCP_JUDGE_KAPPA_URL")
            .unwrap_or_else(|_| "http://localhost:8001/v1".to_string());
        let kappa_model = std::env::var("PGMCP_JUDGE_KAPPA_MODEL")
            .unwrap_or_else(|_| "deepseek-v4-pro".to_string());
        let api_key = std::env::var("PGMCP_JUDGE_API_KEY").ok();
        let pool_k: i32 = std::env::var("PGMCP_JUDGE_POOL_K")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(12);
        let kappa_sample: usize = std::env::var("PGMCP_JUDGE_KAPPA_SAMPLE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10);
        let conceptual = conceptual_queries();
        let score_depth = args.limit.min(10); // ≤ pool_k so every scored hit was graded

        match JudgeClient::new(&primary_url, &primary_model, api_key.clone()) {
            Ok(primary) => {
                eprintln!("[judge] primary {primary_model} @ {primary_url} — smoke test…");
                match primary.smoke().await {
                    Ok(g) => {
                        eprintln!("[judge] primary OK (smoke grade={g}); building κ judge…");
                        let kappa_judge = match JudgeClient::new(&kappa_url, &kappa_model, api_key)
                        {
                            Ok(kj) => match kj.smoke().await {
                                Ok(_) => {
                                    eprintln!("[judge] κ judge {kappa_model} @ {kappa_url} OK");
                                    Some(kj)
                                }
                                Err(e) => {
                                    eprintln!("[judge] κ judge unreachable ({e}); κ skipped");
                                    None
                                }
                            },
                            Err(e) => {
                                eprintln!("[judge] κ judge init failed ({e}); κ skipped");
                                None
                            }
                        };
                        eprintln!(
                            "[judge] grading {} conceptual queries (pool_k={pool_k}, score_depth={score_depth}, κ-sample={kappa_sample})…",
                            conceptual.len()
                        );
                        Some(
                            run_judge_stratum(
                                &live_server,
                                &pool,
                                &conceptual,
                                &primary,
                                kappa_judge.as_ref(),
                                pool_k,
                                score_depth,
                                kappa_sample,
                            )
                            .await,
                        )
                    }
                    Err(e) => {
                        eprintln!(
                            "[judge] primary judge unreachable ({e}); skipping judge stratum"
                        );
                        None
                    }
                }
            }
            Err(e) => {
                eprintln!("[judge] primary judge init failed ({e}); skipping");
                None
            }
        }
    } else {
        None
    };

    let results = CampaignResults {
        note: "pgmcp retrieval-quality campaign. All metrics rank-based. \
                Cross-mode comparison at file granularity (uniform key)."
            .to_string(),
        embedder: embedder_label.to_string(),
        limit: args.limit,
        corpus,
        known_item,
        hnsw_exact_recall_at_ef100,
        ef_search_ablation,
        truncation,
        m2_holdout,
        m2_holdout_chunk,
        m1,
        m1_redacted,
        rerank_strata,
        graph_strata,
        judge,
    };

    // Write JSON artifact.
    if let Some(parent) = std::path::Path::new(&args.out).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let json = serde_json::to_string_pretty(&results)?;
    std::fs::write(&args.out, &json).with_context(|| format!("write {}", args.out))?;

    print_summary(&results);
    eprintln!("\n[done] full results → {}", args.out);
    Ok(())
}

fn print_summary(r: &CampaignResults) {
    println!(
        "\n══════════ pgmcp retrieval-quality summary ({}) ══════════",
        r.embedder
    );
    print_stratum(&r.known_item);
    if let Some(m2) = &r.m2_holdout {
        print_stratum(m2);
    }
    if let Some(m2c) = &r.m2_holdout_chunk {
        print_stratum(m2c);
    }
    if let Some(m1) = &r.m1 {
        print_stratum(m1);
    }
    if let Some(m3) = &r.m1_redacted {
        print_stratum(m3);
    }
    for rr in &r.rerank_strata {
        print_stratum(rr);
    }
    for g in &r.graph_strata {
        print_stratum(g);
    }
    if let Some(j) = &r.judge {
        print_stratum(&j.stratum);
        let rep = &j.report;
        println!(
            "   judge: primary={} graded={} relevant={} grades={:?}",
            rep.primary_model, rep.n_candidates_graded, rep.n_relevant, rep.grade_histogram
        );
        match (rep.kappa_quadratic, &rep.kappa_model) {
            (Some(k), Some(m)) => {
                println!(
                    "   κ (quadratic-weighted, vs {m}, n={}): {:.3}",
                    rep.kappa_n, k
                )
            }
            _ => println!("   κ: n/a (second judge unavailable)"),
        }
    }
    println!(
        "\n── HNSW vs exact (ef=100): mean recall@10 = {:.3} ──",
        r.hnsw_exact_recall_at_ef100
    );
    for p in &r.ef_search_ablation {
        println!(
            "   ef_search={:>3}: recall_vs_exact={:.3}  latency={:.1}ms",
            p.ef_search, p.mean_recall_vs_exact, p.mean_latency_ms
        );
    }
}

fn print_stratum(s: &StratumResult) {
    println!(
        "\n── stratum `{}`  (n={}, complete={}) ──",
        s.label, s.n_queries, s.n_complete
    );
    println!(
        "   {:<10} {:>6} {:>8} {:>8} {:>8} {:>8}",
        "mode", "MRR", "nDCG@10", "R@1", "R@10", "crowd@5"
    );
    for m in &s.per_mode {
        println!(
            "   {:<10} {:>6.3} {:>8.3} {:>8.3} {:>8.3} {:>8.3}",
            m.mode,
            m.mrr,
            m.ndcg_at_10,
            m.recall_at.get(&1).copied().unwrap_or(0.0),
            m.recall_at.get(&10).copied().unwrap_or(0.0),
            m.pattern_crowding_at_5,
        );
    }
    println!("   pairwise (treatment − control), BH-adjusted:");
    for p in &s.pairwise {
        let sig = if p.significant { "*" } else { " " };
        println!(
            "     [{}] {:<9} vs {:<9} {:>9}: Δ={:+.3} δ={:+.3} ({}) p_adj={:.4}{}",
            p.metric,
            p.control,
            p.treatment,
            "",
            p.mean_diff,
            p.cliffs_delta,
            p.effect_magnitude,
            p.wilcoxon_p_adj,
            sig
        );
    }
}
