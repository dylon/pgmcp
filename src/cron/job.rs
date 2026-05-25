//! `CronJob` — testability seam over the existing per-cron free functions.
//!
//! Every heavy maintenance cron in `src/cron/` is currently a free function
//! `pub async fn run_xxx(db: &dyn DbClient, stats: &Arc<StatsTracker>)`. That
//! works, but it bakes the concrete types into every call site and prevents
//! the scheduler from holding `Vec<Arc<dyn CronJob>>` for uniform dispatch.
//!
//! The trait below wraps the existing free functions in named structs and
//! exposes them as a `dyn`-safe surface. Each struct holds zero state — the
//! scheduler clones an `Arc<dyn CronJob>` per dispatch and the body runs
//! against the supplied `SystemContext`. Adding a new cron means: (a) write
//! the free function as today, (b) add a struct + impl here. The scheduler
//! changes nothing.

use std::sync::Arc;

use async_trait::async_trait;

use crate::db::DbClient;
use crate::stats::tracker::StatsTracker;

/// Lightweight envelope over a heavy maintenance cron job.
#[allow(dead_code)]
#[async_trait]
pub trait CronJob: Send + Sync {
    /// Stable name used for logging, telemetry, and the heavy-cron lock key.
    fn name(&self) -> &'static str;

    /// Whether this job acquires the daemon's `heavy_cron` mutex. Heavy
    /// jobs serialize against each other; light jobs (none today) would
    /// not need the lock.
    fn is_heavy(&self) -> bool {
        true
    }

    /// Run the cron body. `db` + `stats` are the only dependencies the
    /// existing free functions take; the SystemContext-shaped richer
    /// version can be added when needed by extending the trait.
    async fn run(&self, db: &dyn DbClient, stats: &Arc<StatsTracker>);
}

/// Adapter for `crate::cron::symbol_extraction::run_symbol_extraction`.
#[allow(dead_code)]
pub struct SymbolExtractionJob;

#[async_trait]
impl CronJob for SymbolExtractionJob {
    fn name(&self) -> &'static str {
        "symbol-extraction"
    }
    async fn run(&self, db: &dyn DbClient, stats: &Arc<StatsTracker>) {
        crate::cron::symbol_extraction::run_symbol_extraction(db, stats).await;
    }
}

/// Adapter for `crate::cron::call_graph::run_call_graph`.
#[allow(dead_code)]
pub struct CallGraphJob;

#[async_trait]
impl CronJob for CallGraphJob {
    fn name(&self) -> &'static str {
        "call-graph"
    }
    async fn run(&self, db: &dyn DbClient, stats: &Arc<StatsTracker>) {
        crate::cron::call_graph::run_call_graph(db, stats, None).await;
    }
}

/// Adapter for `crate::cron::function_metrics::run_function_metrics`.
#[allow(dead_code)]
pub struct FunctionMetricsJob;

#[async_trait]
impl CronJob for FunctionMetricsJob {
    fn name(&self) -> &'static str {
        "function-metrics"
    }
    async fn run(&self, db: &dyn DbClient, stats: &Arc<StatsTracker>) {
        crate::cron::function_metrics::run_function_metrics(db, stats).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_job_trait_is_object_safe() {
        fn _assert_send_sync<T: Send + Sync>() {}
        _assert_send_sync::<Arc<dyn CronJob>>();
    }

    #[test]
    fn each_adapter_reports_the_documented_name() {
        let s: Arc<dyn CronJob> = Arc::new(SymbolExtractionJob);
        let c: Arc<dyn CronJob> = Arc::new(CallGraphJob);
        let f: Arc<dyn CronJob> = Arc::new(FunctionMetricsJob);
        assert_eq!(s.name(), "symbol-extraction");
        assert_eq!(c.name(), "call-graph");
        assert_eq!(f.name(), "function-metrics");
        for job in [s, c, f] {
            assert!(job.is_heavy(), "{} should be a heavy cron", job.name());
        }
    }
}
