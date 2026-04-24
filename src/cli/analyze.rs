//! `analyze [job]` subcommand: run on-demand analysis cron jobs.
//!
//! Subcommands: `similarity`, `topics`, `graph`. Without a sub-job, runs
//! all three.

use std::path::Path;
use std::sync::Arc;

use clap::Subcommand;

use crate::config::{self, Config};
use crate::cron;
use crate::db;
use crate::stats;

#[derive(Subcommand, Clone)]
pub enum AnalyzeJob {
    /// Run only the cross-project similarity scan
    Similarity,
    /// Run only the FCM topic clustering scan (Fuzzy BERTopic)
    Topics,
    /// Run only the graph analysis (import extraction + metrics)
    Graph,
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    config_override: Option<&Path>,
    job: Option<AnalyzeJob>,
    similarity_threshold: Option<f64>,
    similarity_top_k: Option<i32>,
    min_cluster_size: Option<usize>,
    num_clusters: Option<usize>,
    fuzziness: Option<f64>,
) -> anyhow::Result<()> {
    let config = Config::load(config_override)?;
    let pool = db::pool::create_pool(&config.database).await?;
    db::migrations::run_migrations(&pool, &config.vector).await?;

    // Apply CLI overrides to cron config
    let mut cron_config = config.cron.clone();
    if let Some(t) = similarity_threshold {
        cron_config.similarity_threshold = t;
    }
    if let Some(k) = similarity_top_k {
        cron_config.similarity_top_k = k;
    }
    if let Some(s) = min_cluster_size {
        cron_config.topic_min_cluster_size = s;
    }
    if num_clusters.is_some() {
        cron_config.topic_num_clusters = num_clusters;
    }
    if let Some(f) = fuzziness {
        cron_config.topic_fuzziness = f;
    }

    let stats = Arc::new(stats::tracker::StatsTracker::new());
    let db_client: Arc<dyn db::DbClient> = Arc::new(pool.clone());

    match job {
        Some(AnalyzeJob::Similarity) => {
            run_analyze_similarity(db_client.as_ref(), &cron_config, &config.vector, &stats).await;
        }
        Some(AnalyzeJob::Topics) => {
            run_analyze_topics(db_client.as_ref(), &cron_config, &stats).await;
        }
        Some(AnalyzeJob::Graph) => {
            run_analyze_graph(db_client.as_ref(), &stats).await;
        }
        None => {
            run_analyze_similarity(db_client.as_ref(), &cron_config, &config.vector, &stats).await;
            run_analyze_topics(db_client.as_ref(), &cron_config, &stats).await;
            run_analyze_graph(db_client.as_ref(), &stats).await;
        }
    }
    Ok(())
}

async fn run_analyze_similarity(
    db: &dyn db::DbClient,
    cron_config: &config::CronConfig,
    vector_config: &config::VectorConfig,
    stats: &Arc<stats::tracker::StatsTracker>,
) {
    println!(
        "Running similarity scan (threshold={:.2}, top_k={}, ef_search={})...",
        cron_config.similarity_threshold, cron_config.similarity_top_k, vector_config.ef_search,
    );
    let start = std::time::Instant::now();
    cron::similarity::run_similarity_scan(db, cron_config, vector_config.ef_search, stats).await;
    let elapsed = start.elapsed();
    let pairs = stats
        .similarity_pairs_found
        .load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "Similarity scan complete: {} pairs found in {:.1}s",
        pairs,
        elapsed.as_secs_f64(),
    );
}

async fn run_analyze_topics(
    db: &dyn db::DbClient,
    cron_config: &config::CronConfig,
    stats: &Arc<stats::tracker::StatsTracker>,
) {
    println!(
        "Running FCM topic clustering (min_cluster_size={}, K={}, m={:.1})...",
        cron_config.topic_min_cluster_size,
        cron_config
            .topic_num_clusters
            .map(|k| k.to_string())
            .unwrap_or_else(|| "auto".into()),
        cron_config.topic_fuzziness,
    );
    let start = std::time::Instant::now();
    cron::topic_clustering::run_global_topic_scan(db, cron_config, stats).await;
    let elapsed = start.elapsed();
    let topics = stats
        .topics_discovered
        .load(std::sync::atomic::Ordering::Relaxed);
    let noise = stats
        .topic_noise_chunks
        .load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "Topic clustering complete: {} topics, {} noise chunks in {:.1}s",
        topics,
        noise,
        elapsed.as_secs_f64(),
    );
}

async fn run_analyze_graph(db: &dyn db::DbClient, stats: &Arc<stats::tracker::StatsTracker>) {
    println!("Running graph analysis (import extraction + metrics)...");
    let start = std::time::Instant::now();
    // CLI path: no WorkPool available → sequential Brandes. Daemon path
    // passes Some(work_pool) via schedule_maintenance_jobs for parallel.
    cron::graph_analysis::run_graph_analysis(db, stats, None).await;
    let elapsed = start.elapsed();
    let runs = stats
        .graph_build_runs
        .load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "Graph analysis complete: {} runs in {:.1}s",
        runs,
        elapsed.as_secs_f64(),
    );
}
