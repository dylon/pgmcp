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
use pgmcp::embed::{EmbedSource, EmbeddingBackend};
use pgmcp::mcp::server::McpServer;
use pgmcp::quality::retrieval_metrics::{MatchGranularity, QueryMetrics, compute_query_metrics};
use pgmcp::stats::tracker::StatsTracker;

use pgmcp_testing::eval::corpus;
use pgmcp_testing::eval::query::{EvalQuery, GoldTarget, QueryStrategy, known_item_queries};
use pgmcp_testing::eval::runner::{SearchMode, pattern_crowding_at_k, run_mode};
use pgmcp_testing::eval::stats::{AlignedMetric, PairwiseComparison, compare_all_pairs, mean};

const KS: [usize; 4] = [1, 5, 10, 20];
const ALPHA: f64 = 0.05;
const EF_GRID: [i32; 3] = [40, 100, 200];

struct Args {
    gpu: bool,
    limit: i32,
    run_m1: bool,
    m1_targets: usize,
    m1_distractors: usize,
    out: String,
}

fn parse_args() -> Args {
    let mut a = Args {
        gpu: false,
        limit: 20,
        run_m1: false,
        m1_targets: 80,
        m1_distractors: 800,
        out: "target/eval/retrieval_results.json".to_string(),
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--gpu" => a.gpu = true,
            "--m1" => a.run_m1 = true,
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
    m1: Option<StratumResult>,
    m1_redacted: Option<StratumResult>,
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
    let result = run_stratum(&server, &queries, limit, label).await;

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
    label: &str,
) -> StratumResult {
    let modes = SearchMode::all();
    // per_query[qid][mode] = QueryMetrics
    let mut per_query: HashMap<String, HashMap<SearchMode, QueryMetrics>> = HashMap::new();
    let mut crowd: HashMap<SearchMode, Vec<f64>> = HashMap::new();

    for q in queries {
        for mode in modes {
            match run_mode(server, mode, &q.query, q.project.as_deref(), limit).await {
                Ok(hits) => {
                    let m =
                        compute_query_metrics(&hits, &q.gold_items(), MatchGranularity::File, &KS);
                    per_query.entry(q.id.clone()).or_default().insert(mode, m);
                    // Pattern-crowding only over queries whose gold is NOT itself a
                    // pattern file (else a pattern hit is correct, not crowding).
                    if !q.gold.iter().any(|g| g.path.starts_with("src/patterns/")) {
                        crowd
                            .entry(mode)
                            .or_default()
                            .push(pattern_crowding_at_k(&hits, 5));
                    }
                }
                Err(e) => eprintln!("warn[{label}]: {} / {} failed: {e}", q.id, mode.tag()),
            }
        }
    }

    // Queries where ALL modes produced metrics — the paired-comparison set.
    let mut complete: Vec<String> = per_query
        .iter()
        .filter(|(_, m)| modes.iter().all(|md| m.contains_key(md)))
        .map(|(id, _)| id.clone())
        .collect();
    complete.sort();

    // Aligned per-mode vectors for a metric, ordered by `complete`.
    let aligned = |name: &str, f: &dyn Fn(&QueryMetrics) -> f64| -> AlignedMetric {
        let by_mode = modes
            .iter()
            .map(|md| {
                let v: Vec<f64> = complete.iter().map(|id| f(&per_query[id][md])).collect();
                (md.tag().to_string(), v)
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

    // Per-mode means over the complete set.
    let per_mode = modes
        .iter()
        .map(|md| {
            let rr: Vec<f64> = complete
                .iter()
                .map(|id| per_query[id][md].reciprocal_rank)
                .collect();
            let nd: Vec<f64> = complete
                .iter()
                .map(|id| per_query[id][md].ndcg_at(10))
                .collect();
            let s1: Vec<f64> = complete
                .iter()
                .map(|id| {
                    per_query[id][md]
                        .at_k
                        .iter()
                        .find(|a| a.k == 1)
                        .map(|a| a.success)
                        .unwrap_or(0.0)
                })
                .collect();
            let mut recall_at = BTreeMap::new();
            for &k in &KS {
                let rk: Vec<f64> = complete
                    .iter()
                    .map(|id| per_query[id][md].recall_at(k))
                    .collect();
                recall_at.insert(k, mean(&rk));
            }
            ModeMeans {
                mode: md.tag().to_string(),
                n: complete.len(),
                mrr: mean(&rr),
                ndcg_at_10: mean(&nd),
                success_at_1: mean(&s1),
                recall_at,
                pattern_crowding_at_5: mean(crowd.get(md).map(|v| v.as_slice()).unwrap_or(&[])),
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
        n_queries: queries.len(),
        n_complete: complete.len(),
        per_mode,
        pairwise,
        samples,
    }
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
    let known_item = run_stratum(&live_server, &ki.queries, args.limit, "known_item").await;

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
                    Some(run_stratum(&live_server, &m2_queries, args.limit, "m2_holdout").await)
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
        m1,
        m1_redacted,
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
    if let Some(m1) = &r.m1 {
        print_stratum(m1);
    }
    if let Some(m3) = &r.m1_redacted {
        print_stratum(m3);
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
