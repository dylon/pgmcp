//! `concurrency-scan` cron: runs the lock-order + channel deadlock analyses per
//! project, records findings to the `concurrency_findings` ledger (idempotent,
//! provenance-keyed), materializes the bitemporal `lock_order_edges` (feeds the
//! unified-graph `lock_order` arm — Layer 4), snapshots
//! `concurrency_health_history`, and — when `[cron] concurrency_auto_promote` is
//! on — promotes high-severity deadlock findings to `pending` `bug` work items
//! (never `confirmed`; confirmation is user-only).
//!
//! Opt-in, default OFF (`[cron] concurrency_scan_interval_secs = 0`).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::json;
use sqlx::PgPool;
use tracing::{error, info};

use crate::concurrency::findings::ConcurrencyFindingKind;
use crate::concurrency::{self, LockOrderOptions};
use crate::db::queries::{self, FindingAnchor, NewConcurrencyFinding, NewLockEdge, NewWorkItem};
use crate::graph::lock_order::AcqMode;
use crate::graph::petri::ChannelFindingKind;
use crate::stats::tracker::StatsTracker;
use crate::tracker::git_link::FindingSource;

fn mode_str(m: AcqMode) -> &'static str {
    match m {
        AcqMode::Read => "read",
        AcqMode::Write => "write",
    }
}

fn gen_public_id() -> String {
    format!(
        "finding-conc-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    )
}

pub async fn run_or_log(pool: PgPool, stats: Arc<StatsTracker>, auto_promote: bool) {
    let _ = stats.cron_executions.fetch_add(1, Ordering::Relaxed);
    let projects = match queries::list_projects(&pool).await {
        Ok(p) => p,
        Err(e) => {
            stats.cron_panics.fetch_add(1, Ordering::Relaxed);
            error!(error = %e, "concurrency-scan: list_projects failed");
            return;
        }
    };
    let mut total = 0u64;
    for project in &projects {
        match scan_project(&pool, project.id, auto_promote).await {
            Ok(n) => total += n,
            Err(e) => error!(
                project = %project.name,
                error = %e,
                "concurrency-scan: project sweep failed (non-fatal)"
            ),
        }
    }
    info!(
        projects = projects.len(),
        findings = total,
        promote = auto_promote,
        "concurrency-scan complete"
    );
}

/// Promote a finding to a `pending` work item (best-effort: a promotion failure
/// is logged, never propagated, so the ledger write still stands).
async fn promote(
    pool: &PgPool,
    project_id: i32,
    source: FindingSource,
    finding_id: i64,
    provenance_key: &str,
    title: &str,
    body: &str,
    symbol_id: Option<i64>,
    severity: &str,
) {
    let public_id = gen_public_id();
    let item = NewWorkItem {
        public_id: &public_id,
        project_id: Some(project_id),
        kind: source.item_kind(),
        status: "pending", // TRUST BOUNDARY: never pre-`confirmed`.
        title,
        body: Some(body),
        priority: 50,
        severity: Some(severity),
        origin: "agent_write",
        ..Default::default()
    };
    let anchor = FindingAnchor {
        symbol_id,
        ..Default::default()
    };
    match queries::promote_finding(pool, provenance_key, source.as_str(), item, anchor).await {
        Ok((item_id, created)) => {
            if created
                && let Err(e) = queries::set_finding_promoted_item(pool, finding_id, item_id).await
            {
                error!(error = %e, "concurrency-scan: back-patch promoted_item_id failed");
            }
        }
        Err(e) => error!(error = ?e, "concurrency-scan: promote finding failed (non-fatal)"),
    }
}

async fn scan_project(
    pool: &PgPool,
    project_id: i32,
    auto_promote: bool,
) -> Result<u64, sqlx::Error> {
    let lock_findings =
        concurrency::analyze_lock_order(pool, project_id, LockOrderOptions::default()).await?;
    let (chan_findings, _meta) = concurrency::analyze_channels(pool, project_id).await?;

    let mut recorded = 0u64;
    let mut edges: Vec<NewLockEdge> = Vec::new();
    let (mut deadlock_count, mut chan_cycle_count, mut blocked_count) = (0i32, 0i32, 0i32);

    for f in &lock_findings {
        deadlock_count += 1;
        let mut sorted = f.cycle.resources.clone();
        sorted.sort();
        let provenance_key = format!("conc:deadlock_cycle:{project_id}:{}", sorted.join("|"));
        let evidence = json!({
            "resources": f.cycle.resources,
            "public_api_reachable": f.public_api_reachable,
            "edges": f.cycle.edges.iter().map(|e| json!({
                "from": e.from, "to": e.to,
                "from_mode": mode_str(e.from_mode), "to_mode": mode_str(e.to_mode),
                "interprocedural": e.interprocedural, "min_confidence": e.min_confidence,
            })).collect::<Vec<_>>(),
        });
        let severity = f.severity.as_str();
        let title = format!(
            "Lock-order deadlock cycle: {}",
            f.cycle.resources.join(" → ")
        );
        let symbol_id = f.cycle.edges.first().map(|e| e.held_symbol);
        let nf = NewConcurrencyFinding {
            finding_kind: ConcurrencyFindingKind::DeadlockCycle.as_str().to_string(),
            severity: severity.to_string(),
            confidence: f.cycle.min_confidence(),
            provenance_key: provenance_key.clone(),
            symbol_id,
            file_id: None,
            evidence,
            title: title.clone(),
        };
        let finding_id = queries::record_concurrency_finding(pool, project_id, &nf).await?;
        recorded += 1;
        for e in &f.cycle.edges {
            edges.push(NewLockEdge {
                from_key: e.from.clone(),
                to_key: e.to.clone(),
                from_mode: Some(mode_str(e.from_mode).to_string()),
                to_mode: Some(mode_str(e.to_mode).to_string()),
                min_confidence: e.min_confidence,
                interprocedural: e.interprocedural,
            });
        }
        // Promote Critical/High lock cycles (not all-read informational ones).
        if auto_promote && matches!(severity, "critical" | "high") {
            let body = format!(
                "Interprocedural lock-order deadlock cycle over [{}]. \
                 public_api_reachable={}. See `deadlock_cycles` for the witness; \
                 soundness in docs/formal/rocq/LockOrderDeadlock.v (ADR-011).",
                f.cycle.resources.join(", "),
                f.public_api_reachable
            );
            promote(
                pool,
                project_id,
                FindingSource::DeadlockCycle,
                finding_id,
                &provenance_key,
                &title,
                &body,
                symbol_id,
                severity,
            )
            .await;
        }
    }

    for f in &chan_findings {
        let kind = f.kind.as_str();
        let (severity, suffix, promotable) = match f.kind {
            ChannelFindingKind::ChannelCycle => {
                chan_cycle_count += 1;
                let procs = f
                    .processes
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join("|");
                ("critical", format!("procs:{procs}"), true)
            }
            ChannelFindingKind::BlockedRecv => {
                blocked_count += 1;
                (
                    "high",
                    format!("ch:{}", f.channel.clone().unwrap_or_default()),
                    true,
                )
            }
            ChannelFindingKind::OrphanSend => (
                "low",
                format!("ch:{}", f.channel.clone().unwrap_or_default()),
                false,
            ),
        };
        let provenance_key = format!("conc:{kind}:{project_id}:{suffix}");
        let evidence = json!({
            "channel": f.channel,
            "processes": f.processes,
            "waits": f.waits.iter().map(|(id, c)| json!({"symbol_id": id, "waits_on": c})).collect::<Vec<_>>(),
            "detail": f.detail,
        });
        let symbol_id = f.processes.first().copied();
        let nf = NewConcurrencyFinding {
            finding_kind: kind.to_string(),
            severity: severity.to_string(),
            confidence: 0.85,
            provenance_key: provenance_key.clone(),
            symbol_id,
            file_id: None,
            evidence,
            title: f.detail.clone(),
        };
        let finding_id = queries::record_concurrency_finding(pool, project_id, &nf).await?;
        recorded += 1;
        if auto_promote && promotable {
            promote(
                pool,
                project_id,
                FindingSource::ChannelDeadlock,
                finding_id,
                &provenance_key,
                &f.detail,
                &f.detail,
                symbol_id,
                severity,
            )
            .await;
        }
    }

    // Lock contention: record high-contention locks as findings + snapshot the
    // per-lock scores + project max (feeds the forecast / trajectory / digest).
    let contention = queries::lock_contention_ranking(pool, project_id, 50).await?;
    let max_lock_contention = contention
        .iter()
        .map(|r| r.contention_score())
        .fold(0.0f64, f64::max) as f32;
    let mut lock_map = serde_json::Map::new();
    for r in &contention {
        lock_map.insert(r.resource_key.clone(), json!(r.contention_score()));
        if r.distinct_acquirers >= 4 {
            let severity = if r.distinct_acquirers >= 10 {
                "high"
            } else {
                "medium"
            };
            let nf = NewConcurrencyFinding {
                finding_kind: ConcurrencyFindingKind::LockContention.as_str().to_string(),
                severity: severity.to_string(),
                confidence: (r.distinct_acquirers as f32 / 20.0).min(1.0),
                provenance_key: format!("conc:lock_contention:{project_id}:{}", r.resource_key),
                symbol_id: None,
                file_id: None,
                evidence: json!({
                    "resource_key": r.resource_key, "resource_kind": r.resource_kind,
                    "distinct_acquirers": r.distinct_acquirers, "total_acquires": r.total_acquires,
                    "max_pagerank": r.max_pagerank, "contention_score": r.contention_score(),
                }),
                title: format!(
                    "Lock contention: {} ({} acquirers)",
                    r.resource_key, r.distinct_acquirers
                ),
            };
            queries::record_concurrency_finding(pool, project_id, &nf).await?;
            recorded += 1;
        }
    }

    // Materialize the bitemporal lock-order edges (closes ones no longer present)
    // and snapshot health — always, so the forecast sees regular samples.
    queries::refresh_lock_order_edges(pool, project_id, &edges).await?;
    let summary = json!({
        "deadlock_cycles": deadlock_count,
        "channel_cycles": chan_cycle_count,
        "blocked_recv": blocked_count,
        "lock_contention": lock_map,
    });
    queries::insert_concurrency_health(
        pool,
        project_id,
        deadlock_count,
        chan_cycle_count,
        blocked_count,
        max_lock_contention,
        &summary,
    )
    .await?;

    Ok(recorded)
}
