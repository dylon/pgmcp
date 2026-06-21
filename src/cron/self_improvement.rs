//! `self-improvement` cron (ADR-015): idempotently materialize improvement
//! signals into `pending` `idea` work items so the governed self-improvement
//! loop can plan, red-team, and (on the human's plan-review signoff) act on them.
//!
//! Two signals, both mined from the A2A learning ledgers that the
//! `a2a-reflect` pipeline already populates:
//!
//!   - **Recurring outcome failures** — a `(task_kind, approach)` cluster with
//!     `≥ failure_threshold` distinct `failed` outcomes in the lookback window
//!     (`agent_outcomes`) → a `pending` `idea` proposing to improve that approach.
//!   - **Persistently low trust** — an agent whose `agent_trust.importance_prior`
//!     is `≤ low_trust_floor` after `≥ low_trust_min_reports` reports → a
//!     `pending` `idea` proposing to investigate that underperforming approach.
//!
//! IDEMPOTENCY: each proposal has a stable `provenance_key` and goes through the
//! shared [`crate::db::queries::promote_finding`] (a no-op when the key already
//! exists), so re-running the cron never duplicates.
//!
//! TRUST: proposals land in `pending` — NEVER pre-`confirmed`. The cron performs
//! no status transitions and applies nothing; the human plan-review signoff
//! (the structural trust boundary) gates every change.
//!
//! ON by default (`[self_improvement] enabled = true`); a flood guard caps
//! `max_promotions` per signal per run. Light job (two bounded aggregate queries).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{error, info};

use crate::config::SelfImprovementConfig;
use crate::db::queries::{self, FindingAnchor, NewWorkItem};
use crate::stats::tracker::StatsTracker;
use crate::tracker::git_link::FindingSource;

/// One self-improvement sweep. `pool` is an owned `PgPool` (cheaply cloned from
/// `DbClient::pool()`). Best-effort: one signal's failure never aborts the other.
/// Only called when `[self_improvement] enabled` (the daemon gates scheduling).
pub async fn run_or_log(pool: PgPool, stats: Arc<StatsTracker>, cfg: SelfImprovementConfig) {
    let _ = stats.cron_executions.fetch_add(1, Ordering::Relaxed);

    // Resolve the governing tag once (best-effort); proposals are tagged
    // `self-improvement` so the loop can find them. A tag failure is non-fatal —
    // the proposal is still filed, just untagged.
    let tag_id = match queries::upsert_tag(
        &pool,
        "self-improvement",
        "self-improvement",
        None,
        Some("Governed self-improvement proposal (ADR-015)."),
    )
    .await
    {
        Ok(id) => Some(id),
        Err(e) => {
            error!(error = %e, "self-improvement: upsert self-improvement tag failed (proposals will be untagged)");
            None
        }
    };

    let mut created = 0u64;
    match propose_outcome_failures(&pool, &cfg, tag_id).await {
        Ok(n) => created += n,
        Err(e) => {
            stats.cron_panics.fetch_add(1, Ordering::Relaxed);
            error!(error = %e, "self-improvement: outcome-failure scan failed");
        }
    }
    match propose_low_trust(&pool, &cfg, tag_id).await {
        Ok(n) => created += n,
        Err(e) => {
            stats.cron_panics.fetch_add(1, Ordering::Relaxed);
            error!(error = %e, "self-improvement: low-trust scan failed");
        }
    }

    if created > 0 {
        stats
            .findings_promoted
            .fetch_add(created, Ordering::Relaxed);
        info!(
            promoted = created,
            "self-improvement: filed pending idea proposals"
        );
    }
}

/// Recurring `(task_kind, approach)` failure clusters → `pending` `idea` items.
async fn propose_outcome_failures(
    pool: &PgPool,
    cfg: &SelfImprovementConfig,
    tag_id: Option<i64>,
) -> Result<u64, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Cluster {
        project_id: Option<i32>,
        task_kind: String,
        approach: String,
        failures: i64,
        avg_conf: Option<f64>,
    }
    let clusters: Vec<Cluster> = sqlx::query_as::<_, Cluster>(
        "SELECT project_id, task_kind, approach,
                COUNT(*) AS failures, AVG(confidence)::double precision AS avg_conf
         FROM agent_outcomes
         WHERE outcome = 'failed'
           AND created_at >= now() - make_interval(days => $1)
         GROUP BY project_id, task_kind, approach
         HAVING COUNT(*) >= $2
         ORDER BY COUNT(*) DESC
         LIMIT $3",
    )
    .bind(cfg.lookback_days as i32)
    .bind(cfg.failure_threshold)
    .bind(cfg.max_promotions as i64)
    .fetch_all(pool)
    .await?;

    let mut created = 0u64;
    for c in &clusters {
        // Stable key: source + project + task_kind + approach (NOT the count,
        // which drifts run-to-run). One open proposal per failing approach.
        let provenance_key = format!(
            "{}:outcome_failure:{}:{}:{}",
            FindingSource::SelfImprovement.as_str(),
            c.project_id
                .map(|p| p.to_string())
                .as_deref()
                .unwrap_or("_"),
            c.task_kind,
            c.approach
        );
        let title = format!(
            "Improve approach \"{}\" for \"{}\" tasks",
            c.approach, c.task_kind
        );
        let body = format!(
            "Auto-proposed by the self-improvement cron (ADR-015) from the A2A \
             outcome ledger: the approach `{}` has {} recent `failed` outcomes \
             on `{}` tasks (mean confidence {:.2}). Plan an improvement to the \
             skill / prompt / tool behind this approach; the governed loop will \
             red-team it to convergence and gate it on your plan-review signoff.",
            c.approach,
            c.failures,
            c.task_kind,
            c.avg_conf.unwrap_or(0.0)
        );
        if file_proposal(
            pool,
            &provenance_key,
            c.project_id,
            &title,
            &body,
            30,
            tag_id,
        )
        .await
        {
            created += 1;
        }
    }
    Ok(created)
}

/// Persistently low-trust agents → `pending` `idea` items.
async fn propose_low_trust(
    pool: &PgPool,
    cfg: &SelfImprovementConfig,
    tag_id: Option<i64>,
) -> Result<u64, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct LowTrust {
        agent_id: String,
        importance_prior: f32,
        reports_total: i64,
    }
    let rows: Vec<LowTrust> = sqlx::query_as::<_, LowTrust>(
        "SELECT agent_id, importance_prior, reports_total
         FROM agent_trust
         WHERE importance_prior <= $1 AND reports_total >= $2
         ORDER BY importance_prior ASC
         LIMIT $3",
    )
    .bind(cfg.low_trust_floor as f32)
    .bind(cfg.low_trust_min_reports)
    .bind(cfg.max_promotions as i64)
    .fetch_all(pool)
    .await?;

    let mut created = 0u64;
    for r in &rows {
        let provenance_key = format!(
            "{}:low_trust:{}",
            FindingSource::SelfImprovement.as_str(),
            r.agent_id
        );
        let title = format!("Investigate underperforming agent \"{}\"", r.agent_id);
        let body = format!(
            "Auto-proposed by the self-improvement cron (ADR-015): agent `{}` \
             carries a low trust prior ({:.2}) after {} reports — its approaches \
             are not converging. Plan an investigation/improvement (its model, \
             prompt, or specialty routing); the governed loop gates the fix on \
             your plan-review signoff.",
            r.agent_id, r.importance_prior, r.reports_total
        );
        if file_proposal(pool, &provenance_key, None, &title, &body, 20, tag_id).await {
            created += 1;
        }
    }
    Ok(created)
}

/// Idempotently file one `pending` `idea` proposal and tag it `self-improvement`.
/// Returns true iff a NEW item was created (a re-promotion of a known signal
/// returns false). promote_finding / tag errors are logged inline (non-fatal),
/// matching the `findings-promotion` cron.
async fn file_proposal(
    pool: &PgPool,
    provenance_key: &str,
    project_id: Option<i32>,
    title: &str,
    body: &str,
    priority: i32,
    tag_id: Option<i64>,
) -> bool {
    let public_id = format!(
        "improve-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let item = NewWorkItem {
        public_id: &public_id,
        project_id,
        kind: FindingSource::SelfImprovement.item_kind(), // "idea"
        status: "pending",
        title,
        body: Some(body),
        priority,
        origin: "agent_write",
        ..Default::default()
    };
    match queries::promote_finding(
        pool,
        provenance_key,
        FindingSource::SelfImprovement.as_str(),
        item,
        FindingAnchor::default(),
    )
    .await
    {
        Ok((item_id, was_created)) => {
            if was_created {
                if let Some(tid) = tag_id
                    && let Err(e) =
                        queries::tag_work_item(pool, item_id, tid, Some("self_improvement_cron"))
                            .await
                {
                    error!(error = %e, item_id, "self-improvement: tag attach failed");
                }
                true
            } else {
                false
            }
        }
        Err(e) => {
            error!(error = ?e, key = %provenance_key, "self-improvement: promote proposal failed");
            false
        }
    }
}
