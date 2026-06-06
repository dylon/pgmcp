//! `quality-history` cron: snapshot each project's quality GPAs into
//! `quality_report_history` so the trend/forecast tools and the proactive digest
//! can read a *trajectory* instead of a single point. The v9 table has existed
//! since the quality subsystem shipped, but nothing ever populated it — every
//! metric was a snapshot. This cron closes that gap.
//!
//! Heavy: it fans out the quality collectors via [`crate::quality::aggregate`],
//! so it runs behind the heavy-cron gate in `scheduler.rs` (interval-gated on
//! `quality_history_interval_secs > 0`, default 6h).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tracing::{info, warn};

use crate::context::SystemContext;
use crate::quality::aggregate::aggregate_for_project;
use crate::quality::report::ReportOptions;
use crate::stats::tracker::StatsTracker;

/// Per-project aggregation timeout (seconds). Matches the default tool timeout
/// the `quality_report` tool uses.
const QUALITY_HISTORY_TOOL_TIMEOUT_SECS: u64 = 30;

/// Snapshot every indexed project's quality GPAs into `quality_report_history`.
/// Best-effort per project (one project's failure never aborts the sweep).
pub async fn run_or_log(ctx: SystemContext, stats: Arc<StatsTracker>) {
    stats.quality_history_runs.fetch_add(1, Ordering::Relaxed);
    let Some(pool) = ctx.db().pool().cloned() else {
        warn!(
            job = "quality-history",
            "skipping run: DbClient has no pool"
        );
        return;
    };
    let projects = match crate::db::queries::list_projects(&pool).await {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "quality-history: list_projects failed");
            return;
        }
    };

    let mut snapshots = 0u64;
    for project in &projects {
        // Findings/fixes are not persisted (only the pillar GPAs are), so skip
        // computing them; trend_points=0 skips the read-back strip.
        let opts = ReportOptions {
            include_findings: false,
            compute_findings: false,
            include_recommended_fixes: false,
            trend_points: 0,
            ..Default::default()
        };
        match aggregate_for_project(
            &ctx,
            project.id,
            &project.name,
            opts,
            QUALITY_HISTORY_TOOL_TIMEOUT_SECS,
        )
        .await
        {
            Ok(report) => {
                crate::quality::history::insert_history(&pool, project.id, &report).await;
                snapshots += 1;
            }
            Err(e) => {
                warn!(
                    error = %e,
                    project = %project.name,
                    "quality-history: aggregate failed (non-fatal)"
                );
            }
        }
    }
    stats
        .quality_history_snapshots
        .fetch_add(snapshots, Ordering::Relaxed);
    if snapshots > 0 {
        info!(
            snapshots,
            projects = projects.len(),
            "quality-history snapshot complete"
        );
    }
}
