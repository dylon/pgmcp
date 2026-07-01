//! `trigger_cron` — operator-facing on-demand cron run.
//!
//! The heavy crons (symbol-extraction, call-graph, function-metrics)
//! have a Ready-relative delay (default 30 min) and a steady-state
//! interval (default 2 h). Freshly-started daemons therefore return
//! empty results from `dead_code_reachability` / `naming_consistency`
//! until that delay elapses. This tool lets the operator trigger an
//! immediate run when the data is needed sooner.
//!
//! Safety: each invocation acquires the heavy-cron lock non-blocking
//! (`try_lock`). If a heavy cron is already executing, the call returns
//! `{ status: "busy", retry_after_secs: 60 }` rather than queueing.
//! There's no rate limiting beyond that — heavy crons are themselves
//! the bottleneck and the lock already serialises them.

#![allow(unused_imports)]

use std::sync::Arc;
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::TriggerCronParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn tool_trigger_cron(
    ctx: &SystemContext,
    params: TriggerCronParams,
) -> Result<CallToolResult, McpError> {
    let job = params.job.trim();
    let project = params
        .project
        .as_deref()
        .map(str::trim)
        .filter(|project| !project.is_empty())
        .map(str::to_string);

    tracing::debug!(tool = "trigger_cron", job = %job, "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    if job.is_empty() {
        return Err(McpError::invalid_params("job must be non-empty", None));
    }

    const VALID_JOBS: &[&str] = &[
        "symbol-extraction",
        "call-graph",
        "function-metrics",
        "graph-analysis",
        "a2a-reflect",
        "msm-calibrate",
        "fuzzy-sync",
        "target-cleanup",
        "security-scan",
        "findings-promotion",
        "topic-clustering",
        "topics-size-history",
        "code-raptor",
        "topic-dendrogram",
        "memory-raptor",
    ];
    if !VALID_JOBS.contains(&job) {
        return Err(McpError::invalid_params(
            format!(
                "Unknown job {job:?}. Valid: symbol-extraction | call-graph | function-metrics | graph-analysis | a2a-reflect | msm-calibrate | fuzzy-sync | target-cleanup | security-scan | findings-promotion | topic-clustering | topics-size-history | code-raptor | topic-dendrogram | memory-raptor"
            ),
            None,
        ));
    }

    let _heavy_guard = match ctx.heavy_cron_lock().try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            // Busy = lock held by another heavy run; nothing executed here, so
            // there is no run to record (the busy response is not a cron run).
            return json_result(&json!({
                "job": job,
                "project": project,
                "status": "busy",
                "retry_after_secs": 60,
                "guidance": "Another heavy cron is already running. Retry after it completes; trigger_cron never queues heavy work.",
            }));
        }
    };
    let _cron_flag = crate::cron::scheduler::HeavyCronFlag::new(Arc::clone(ctx.stats()));

    // Record this manual run in cron_run_history (ADR-018). The guard drops at
    // function exit and writes one row: Ok on a returned result, Failed on an
    // Err (a `?`-propagated arm error), Panicked on an unwind.
    let mut run = crate::cron::history::CronRunGuard::new(
        ctx.cron_history().clone(),
        job,
        crate::cron::history::CronTriggerSource::Manual,
        project.clone(),
    );
    // A job arm may surface per-run counters (e.g. target-cleanup's reclaimed
    // bytes) so the manual run's `cron_run_history.counters` matches what the
    // scheduled path records via `spawn_recorded_with` — instead of an empty `{}`.
    let mut counters: Option<serde_json::Value> = None;
    let result = trigger_cron_dispatch(ctx, job, project, &mut counters).await;
    match &result {
        Ok(_) => match counters {
            Some(c) => run.ok_with(c),
            None => run.ok(),
        },
        Err(e) => run.fail(format!("{e:?}")),
    }
    result
}

/// Dispatch a validated `trigger_cron` job to its cron body. Split out from
/// [`tool_trigger_cron`] so the latter can wrap the whole dispatch in a single
/// [`crate::cron::history::CronRunGuard`] (recording the manual run) without
/// threading the run outcome through all 14 arms.
async fn trigger_cron_dispatch(
    ctx: &SystemContext,
    job: &str,
    project: Option<String>,
    counters_out: &mut Option<serde_json::Value>,
) -> Result<CallToolResult, McpError> {
    let db = ctx.db();
    let stats = ctx.stats();
    match job {
        "target-cleanup" => {
            // Disk reclamation: tiered `target/` removal + provenance-first
            // tmp sweep. Honors the live `[cron.target_cleanup]` config, so a
            // manual run produces a dry-run manifest while `dry_run = true` and
            // actually reclaims once armed. An optional `project` filter scopes
            // the sweep by project name / path substring.
            let cfg = ctx.config().load().cron.target_cleanup.clone();
            if let Some(pool) = db.pool().cloned() {
                let report = crate::cron::target_cleanup::run_target_cleanup(
                    &pool,
                    &cfg,
                    project.as_deref(),
                )
                .await;
                report.log_summary();
                // Surface the reclamation counts to the manual run's history row
                // (D1: parity with the scheduled `spawn_recorded_with` path).
                *counters_out = Some(report.to_counters());
                json_result(&json!({
                    "job": job,
                    "project": project,
                    "status": "completed",
                    "dry_run": report.dry_run,
                    "targets_scanned": report.targets_scanned,
                    "total_bytes": report.total_bytes(),
                    "tmp_files_removed": report.tmp_files_removed,
                    "tmp_protected_live": report.tmp_protected_live,
                    "manifest": report.manifest_path,
                    "guidance": "Cleanup ran. Review the manifest under $XDG_STATE_HOME/pgmcp/target-cleanup/ (else ~/.local/state/pgmcp/target-cleanup/). While dry_run=true nothing was deleted; set [cron.target_cleanup] dry_run=false to arm actual removal.",
                }))
            } else {
                json_result(&json!({
                    "job": job,
                    "status": "skipped",
                    "reason": "DbClient has no PgPool (target-cleanup needs Postgres)",
                }))
            }
        }
        "security-scan" => {
            // External security-scanner sweep over the indexed projects (the
            // opt-in cron's on-demand counterpart). Honors the live
            // [security_scan] config; an optional `project` filter scopes by
            // name / path substring. Findings land in external_scanner_findings.
            let cfg = ctx.config().load().security_scan.clone();
            if let Some(pool) = db.pool().cloned() {
                let report =
                    crate::cron::security_scan::run_security_scan(&pool, &cfg, project.as_deref())
                        .await;
                report.log_summary();
                json_result(&json!({
                    "job": job,
                    "project": project,
                    "status": "completed",
                    "projects_scanned": report.projects_scanned,
                    "findings_upserted": report.findings_upserted,
                    "findings_resolved": report.findings_resolved,
                    "runs_ok": report.runs_ok,
                    "runs_timeout": report.runs_timeout,
                    "runs_error": report.runs_error,
                    "scanners_available": report.scanners_available,
                    "scanners_missing": report.scanners_missing,
                    "guidance": "Scanners ran over the indexed projects; findings are in external_scanner_findings (query/refresh via the security_scan tool). Enable [tracker] auto_promote_findings and run trigger_cron job=\"findings-promotion\" to materialize high/critical findings as pending bugs.",
                }))
            } else {
                json_result(&json!({
                    "job": job,
                    "status": "skipped",
                    "reason": "DbClient has no PgPool (security-scan needs Postgres)",
                }))
            }
        }
        "findings-promotion" => {
            // Materialize high-signal analytic findings (bug_prediction,
            // documented_tech_debt, deadlock cycles, security_scan) into pending
            // work items for opted-in projects ([tracker] auto_promote_findings).
            if let Some(pool) = db.pool().cloned() {
                crate::cron::findings_promotion::run_or_log(pool, Arc::clone(stats)).await;
                json_result(&json!({
                    "job": job,
                    "status": "completed",
                    "guidance": "Promotion swept the opted-in projects. New pending items (if any) are in the tracker — triage bugs with work_item_triage.",
                }))
            } else {
                json_result(&json!({
                    "job": job,
                    "status": "skipped",
                    "reason": "DbClient has no PgPool (findings-promotion needs Postgres)",
                }))
            }
        }
        "symbol-extraction" => {
            match project.as_deref() {
                Some(p) => {
                    crate::cron::symbol_extraction::run_symbol_extraction_for_project(
                        db.as_ref(),
                        stats,
                        p,
                    )
                    .await
                }
                None => {
                    crate::cron::symbol_extraction::run_symbol_extraction(db.as_ref(), stats).await
                }
            }
            json_result(&json!({
                "job": job,
                "project": project,
                "status": "completed",
                "guidance": "Symbols populated. dead_code_reachability and naming_consistency should now return populated results. For end-to-end call-graph closure, also run trigger_cron job=\"call-graph\".",
            }))
        }
        "call-graph" => {
            // Manual trigger: no general WorkPool in scope, so betweenness runs
            // sequentially (gated by DENSE_CENTRALITY_MAX_NODES in the cron).
            match project.as_deref() {
                Some(p) => {
                    crate::cron::call_graph::run_call_graph_for_project(db.as_ref(), stats, None, p)
                        .await
                }
                None => crate::cron::call_graph::run_call_graph(db.as_ref(), stats, None).await,
            }
            json_result(&json!({
                "job": job,
                "project": project,
                "status": "completed",
                "guidance": "Call graph populated. dead_code_reachability now uses real symbol_references edges.",
            }))
        }
        "function-metrics" => {
            match project.as_deref() {
                Some(p) => {
                    crate::cron::function_metrics::run_function_metrics_for_project(
                        db.as_ref(),
                        stats,
                        p,
                    )
                    .await
                }
                None => {
                    crate::cron::function_metrics::run_function_metrics(db.as_ref(), stats).await
                }
            }
            // Shadow-ASR channel (Phase D2b): project-scoped effect distribution.
            let effect_breakdown = match ctx.db().pool() {
                Some(pool) => {
                    let pid = crate::mcp::tools::sema_helpers::effects::project_id_opt(
                        pool,
                        project.as_deref(),
                    )
                    .await;
                    crate::mcp::tools::sema_helpers::effects::effect_breakdown_json(pool, pid).await
                }
                None => serde_json::json!({}),
            };

            json_result(&json!({
            "effect_breakdown": effect_breakdown,
                    "job": job,
                    "project": project,
                    "status": "completed",
                    "guidance": "Function metrics populated (cyclomatic, cognitive, Halstead, NPath, MI).",
                }))
        }
        "a2a-reflect" => {
            // Part A phase A4: consensus-gate peer outcomes into the shared
            // scope and promote the strongest agreed practices to durable
            // mandates. On-demand counterpart to the off-by-default cron.
            let pool = ctx
                .db()
                .pool()
                .ok_or_else(|| McpError::internal_error("no pool available", None))?;
            let cfg = ctx.config().load().a2a.reflection.clone();
            let extractor = ctx.llm_extractor();
            let report = crate::a2a::best_practices::run_cross_agent_reflection(
                pool,
                stats,
                extractor.as_deref(),
                &cfg,
            )
            .await
            .map_err(|e| McpError::internal_error(format!("a2a-reflect failed: {e}"), None))?;
            json_result(&json!({
                "job": job,
                "status": "completed",
                "consensus_groups": report.consensus_groups,
                "scopes_reflected": report.scopes_reflected,
                "mandates_promoted": report.mandates_promoted,
                "guidance": "Agreed peer best practices promoted to durable mandates; they re-inject via the UserPromptSubmit hook.",
            }))
        }
        "msm-calibrate" => {
            // Part E (closed MSM loop): refresh trajectory success labels from
            // explicit outcomes, then re-tune the adaptive split/merge cost c
            // for cohort separation (LOO precision-guarded) and persist it.
            let pool = ctx
                .db()
                .pool()
                .ok_or_else(|| McpError::internal_error("no pool available", None))?;
            use crate::fuzzy::trajectory_index::{
                DEFAULT_MSM_C, calibrate_adaptive_c, label_trajectories_from_outcomes, load_msm_c,
                loo_accuracy, store_msm_c,
            };
            let labeled = label_trajectories_from_outcomes(pool)
                .await
                .map_err(|e| McpError::internal_error(format!("label step: {e}"), None))?;
            let cohort = |success: bool| async move {
                sqlx::query_as::<_, (i64, Vec<f64>)>(
                    "SELECT id, encoded_series FROM agent_trajectories
                     WHERE success = $1 AND cardinality(encoded_series) > 0",
                )
                .bind(success)
                .fetch_all(pool)
                .await
            };
            let success = cohort(true)
                .await
                .map_err(|e| McpError::internal_error(format!("success cohort: {e}"), None))?;
            let fail = cohort(false)
                .await
                .map_err(|e| McpError::internal_error(format!("fail cohort: {e}"), None))?;
            let prev_c = load_msm_c(pool).await.unwrap_or(DEFAULT_MSM_C);
            let new_c = calibrate_adaptive_c(&success, &fail, prev_c, 64);
            let _ = store_msm_c(pool, new_c).await;
            json_result(&json!({
                "job": job,
                "status": "completed",
                "newly_labeled": labeled,
                "success_cohort": success.len(),
                "fail_cohort": fail.len(),
                "previous_c": prev_c,
                "calibrated_c": new_c,
                "loo_accuracy": loo_accuracy(&success, &fail, new_c),
                "guidance": "Adaptive MSM cost re-tuned for cohort separation; the RLM strategy chooser (a2a_pattern_recursive) now uses it.",
            }))
        }
        "fuzzy-sync" => {
            // Rebuild the per-project symbol/path/commit + durable-mandate fuzzy
            // tries from PostgreSQL — the on-demand counterpart to the fuzzy-sync
            // cron. Clone config values before the await so the ArcSwap guard is
            // not held across it.
            let pool = ctx
                .db()
                .pool()
                .ok_or_else(|| McpError::internal_error("no pool available", None))?;
            let (data_dir, max_disk_bytes, eviction_cfg) = {
                let cfg = ctx.config().load();
                (
                    cfg.fuzzy.data_dir.clone(),
                    cfg.fuzzy.max_disk_bytes,
                    cfg.fuzzy.eviction_config(),
                )
            };
            let report = crate::cron::fuzzy_sync::run_fuzzy_sync(
                pool,
                &data_dir,
                max_disk_bytes,
                eviction_cfg,
                std::sync::Arc::clone(stats),
            )
            .await
            .map_err(|e| McpError::internal_error(format!("fuzzy-sync failed: {e}"), None))?;
            json_result(&json!({
                "job": job,
                "status": "completed",
                "symbols_synced": report.symbols_synced,
                "paths_synced": report.paths_synced,
                "commits_synced": report.commits_synced,
                "durable_mandates_synced": report.durable_mandates_synced,
                "concepts_synced": report.concepts_synced,
                "guidance": "Per-project symbol/path/commit + global durable-mandate & ontology-concept fuzzy tries rebuilt from PG.",
            }))
        }
        "graph-analysis" => {
            // Rebuild code_graph_edges (import / co-change / semantic) on demand.
            // Run AFTER symbol-extraction so the freshly written `import_use`
            // refs materialize into import edges — this is how the post-fix
            // import-graph backfill is forced without a daemon restart.
            crate::cron::graph_analysis::run_graph_analysis(db.as_ref(), stats, None).await;
            json_result(&json!({
                "job": job,
                "status": "completed",
                "guidance": "Import/co-change/semantic edges rebuilt from symbol_references. Repairs dependency_graph / coupling_cohesion_report / architecture_* once import_use refs exist (run symbol-extraction first).",
            }))
        }
        "topic-clustering" => {
            // Topic engine (default: graph-hybrid per-project + global roll-up +
            // hierarchy + LLM labels), with the degeneracy gate. Forces an
            // immediate refresh without waiting for the 12h interval / ready delay.
            let config = ctx.config().load();
            crate::cron::topic_clustering::run_global_topic_scan(
                db.as_ref(),
                &config.cron,
                stats,
                ctx.lifecycle(),
            )
            .await;
            json_result(&json!({
                "job": job,
                "status": "completed",
                "method": config.cron.topic_clustering_method.clone(),
                "topics_discovered": stats.topics_discovered.load(Ordering::Relaxed),
                "degenerate_refusals": stats.topic_degenerate_refusals.load(Ordering::Relaxed),
                "guidance": "Topics recomputed. discover_topics (per-project scope='project:NAME' + a 'global' roll-up) and topic quality (orient health / pgmcp_metadata['topics_quality']) are refreshed. The degeneracy gate preserves prior topics if the new model is degenerate.",
            }))
        }
        "topics-size-history" => {
            // Snapshot current per-topic sizes into the bounded
            // pgmcp_metadata['topics_size_history'] series read by topic_trends.
            if let Some(pool) = db.pool() {
                crate::cron::topics_size_history::run_or_log(pool).await;
            }
            json_result(&json!({
                "job": job,
                "status": "completed",
                "guidance": "Per-topic size snapshot appended to topics_size_history. topic_trends (mode=longitudinal) needs ≥2 snapshots over time to compute growth/decline.",
            }))
        }
        "code-raptor" => {
            // Per-project RAPTOR summary tree (code_summary_tree); powers
            // code_raptor_search.
            crate::cron::code_raptor::run_code_raptor(db.as_ref(), stats, ctx.lifecycle()).await;
            json_result(&json!({
                "job": job,
                "status": "completed",
                "summaries_written": stats.code_raptor_summaries_written.load(Ordering::Relaxed),
                "guidance": "code_summary_tree rebuilt per project (FCM clusters + summaries).",
            }))
        }
        "topic-dendrogram" => {
            // Hierarchical-agglomerative topic dendrogram (topic_dendrograms);
            // powers dendrogram_topic_hierarchy. Large projects are
            // strided-subsampled to avoid the O(n²) distance-matrix OOM.
            if let Some(pool) = db.pool() {
                match crate::cron::topic_dendrogram::run_pass(pool, stats).await {
                    Ok(report) => json_result(&json!({
                        "job": job,
                        "status": "completed",
                        "projects_processed": report.projects_processed,
                        "topics_generated": report.topics_generated,
                        "errors": report.errors,
                        "guidance": "topic_dendrograms rebuilt; powers dendrogram_topic_hierarchy.",
                    })),
                    Err(e) => json_result(&json!({
                        "job": job,
                        "status": "error",
                        "error": e.to_string(),
                    })),
                }
            } else {
                json_result(&json!({
                    "job": job,
                    "status": "skipped",
                    "reason": "DbClient has no PgPool (topic-dendrogram needs Postgres)",
                }))
            }
        }
        "memory-raptor" => {
            // Memory-server RAPTOR: recursive LLM summarization over the agent
            // memory_observations knowledge graph → memory_summary_tree (powers
            // memory_raptor_search). Loads the configured local LLM for this run.
            let backend = ctx.config().load().cron.topic_llm_backend.clone();
            let extractor = match crate::llm::parse_backend_choice(&backend)
                .and_then(crate::llm::make_extractor)
            {
                Ok(Some(e)) => {
                    let arc: Arc<dyn crate::llm::LlmExtractor> = Arc::from(e);
                    arc
                }
                Ok(None) => {
                    return json_result(&json!({
                        "job": job,
                        "status": "skipped",
                        "reason": format!("LLM backend {backend:?} is disabled; set [cron] topic_llm_backend to qwen3-4b or qwen3-8b"),
                    }));
                }
                Err(e) => {
                    return json_result(&json!({
                        "job": job,
                        "status": "error",
                        "error": format!("LLM extractor load failed: {e}"),
                    }));
                }
            };
            if let Some(pool) = db.pool().cloned() {
                crate::cron::memory_raptor::run_or_log(
                    Arc::new(pool),
                    Arc::clone(stats),
                    extractor,
                )
                .await;
                json_result(&json!({
                    "job": job,
                    "status": "completed",
                    "guidance": "memory_summary_tree rebuilt from memory_observations; powers memory_raptor_search. Heavy (loads the local LLM); run on demand.",
                }))
            } else {
                json_result(&json!({
                    "job": job,
                    "status": "skipped",
                    "reason": "DbClient has no PgPool (memory-raptor needs Postgres)",
                }))
            }
        }
        _ => unreachable!("validated trigger_cron job must match a branch"),
    }
}
