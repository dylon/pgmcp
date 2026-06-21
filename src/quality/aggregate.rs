//! The aggregator: fan out every finding-collector, compute the three pillars,
//! and assemble a [`QualityReport`].
//!
//! Fan-out is two-wave: table-backed collectors run together via `join_all`;
//! the full-file *content* scanners are bounded with `buffer_unordered(4)` so
//! at most four whole-file-body scans are resident at once (a memory bound, not
//! a latency one). Each collector is wrapped in a per-tool timeout; on
//! error/timeout its category gets one `Info` placeholder so the report is
//! always complete.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use chrono::Utc;
use futures::stream::{self, StreamExt};
use rmcp::ErrorData as McpError;
use sqlx::PgPool;

use super::collectors::{
    architecture as arch_c, code_health as ch_c, concurrency as cc_c, dependency as dep_c,
    duplication as dup_c, hygiene as hy_c, security as sec_c, tests_docs as td_c,
};
use super::report::{
    DimensionScore, PillarReport, PillarTrend, QualityReport, ReportOptions, ToolOutcome, ToolRun,
    finding_density,
};
use crate::context::SystemContext;
use crate::mcp::tools::sota_helpers::{pool_or_err, project_id_or_err};
use crate::mcp::tools::tool_architecture_quality::collect_architecture_dimensions;
use crate::mcp::tools::tool_engineering_scorecard::collect_engineering_analysis;
use crate::quality::findings::{Finding, FindingCategory, Pillar, Severity};

/// Default per-collector timeout.
pub const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 60;

type CFut<'a> = Pin<Box<dyn Future<Output = Result<Vec<Finding>, McpError>> + Send + 'a>>;

/// Build the full graded report for `project_name`.
pub async fn aggregate(
    ctx: &SystemContext,
    project_name: &str,
    options: ReportOptions,
    per_tool_timeout_secs: u64,
) -> Result<QualityReport, McpError> {
    let project_name = project_name.trim();
    let project_id = project_id_or_err(ctx, project_name).await?;
    aggregate_for_project(
        ctx,
        project_id,
        project_name,
        options,
        per_tool_timeout_secs,
    )
    .await
}

/// Build the full graded report for an already-resolved project id. Internal
/// cron callers use this path after listing concrete projects so duplicate
/// display names cannot make snapshots disappear.
pub async fn aggregate_for_project(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
    options: ReportOptions,
    per_tool_timeout_secs: u64,
) -> Result<QualityReport, McpError> {
    let pool = pool_or_err(ctx)?;

    let file_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM indexed_files WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(pool)
            .await
            .unwrap_or(0);

    // ── Pillar dimensions (Engineering + Architecture base) ──────────────
    let eng = collect_engineering_analysis(ctx, project_id).await?;
    let mut arch_dims = collect_architecture_dimensions(ctx, project_id).await?;
    arch_dims.push(oo_coupling_dim(pool, project_id).await);
    arch_dims.push(propagation_cost_dim(pool, project_id).await);

    // ── Fan out the finding collectors ───────────────────────────────────
    let (tool_runs, findings) = if options.compute_findings {
        run_collectors(ctx, project_id, project_name, per_tool_timeout_secs).await
    } else {
        (Vec::new(), Vec::new())
    };

    // ── Security pillar dims (derived from collected findings + advisories) ─
    let sec_dims = if options.compute_findings {
        security_dims(pool, &findings, file_count).await
    } else {
        security_dims_skipped()
    };

    // ── finding_density per pillar (over ALL findings, pre display-filter) ─
    let mut eng_dims = eng.dimensions;
    eng_dims.push(finding_density_dimension(
        options.compute_findings,
        &findings,
        Pillar::Engineering,
        file_count,
        "Severity-weighted finding load (Engineering categories)",
    ));
    arch_dims.push(finding_density_dimension(
        options.compute_findings,
        &findings,
        Pillar::Architecture,
        file_count,
        "Severity-weighted finding load (Architecture categories)",
    ));
    let mut sec_dims = sec_dims;
    sec_dims.push(finding_density_dimension(
        options.compute_findings,
        &findings,
        Pillar::Security,
        file_count,
        "Severity-weighted finding load (Security findings)",
    ));

    let pillars = vec![
        PillarReport {
            pillar: Pillar::Engineering,
            dimensions: eng_dims,
        },
        PillarReport {
            pillar: Pillar::Architecture,
            dimensions: arch_dims,
        },
        PillarReport {
            pillar: Pillar::Security,
            dimensions: sec_dims,
        },
    ];

    // ── Trend = persisted history + this run's current GPA appended ──────
    let mut trend = super::history::recent_gpas(pool, project_id, options.trend_points).await;
    if options.trend_points > 0 {
        append_current_point(&mut trend, &pillars);
    }

    // ── Effect breakdown (Engineering enrichment) ────────────────────────
    let effect_breakdown = effect_breakdown_value(pool, project_id).await;

    Ok(QualityReport {
        project: project_name.to_string(),
        computed_at: Utc::now(),
        pgmcp_version: env!("CARGO_PKG_VERSION").to_string(),
        pillars,
        findings,
        orr: eng.orr,
        effect_breakdown,
        tool_runs,
        trend,
        options,
    })
}

fn finding_density_dimension(
    compute_findings: bool,
    findings: &[Finding],
    pillar: Pillar,
    file_count: i64,
    description: &'static str,
) -> DimensionScore {
    if compute_findings {
        DimensionScore::present(
            "finding_density",
            description,
            finding_density(findings, pillar, file_count),
        )
    } else {
        DimensionScore::absent(
            "finding_density",
            format!("{description} - N/A (finding collectors skipped)"),
        )
    }
}

/// Append this run's per-pillar GPA as the newest trend point (so the strip and
/// the delta column include the current run; persistence happens separately).
fn append_current_point(trend: &mut Vec<PillarTrend>, pillars: &[PillarReport]) {
    for pr in pillars {
        let Some(gpa) = pr.gpa() else { continue };
        match trend.iter_mut().find(|t| t.pillar == pr.pillar) {
            Some(t) => t.gpas.push(gpa),
            None => trend.push(PillarTrend {
                pillar: pr.pillar,
                gpas: vec![gpa],
            }),
        }
    }
}

/// CBO surrogate: avg distinct target files referenced per source file. Absent
/// when symbol-reference data is missing.
async fn oo_coupling_dim(pool: &PgPool, project_id: i32) -> DimensionScore {
    let avg: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(cbo)::DOUBLE PRECISION FROM (
            SELECT sr.source_file_id, COUNT(DISTINCT sr.target_file_id) AS cbo
            FROM symbol_references sr
            JOIN indexed_files f ON f.id = sr.source_file_id
            WHERE f.project_id = $1 AND sr.target_file_id IS NOT NULL
            GROUP BY sr.source_file_id
         ) t",
    )
    .bind(project_id)
    .fetch_optional(pool)
    .await
    .unwrap_or(None)
    .flatten();
    match avg {
        Some(cbo) => DimensionScore::present(
            "oo_coupling",
            "Avg coupling-between-objects (distinct referenced files)",
            100.0 * (1.0 - (cbo / 20.0).clamp(0.0, 1.0)),
        ),
        None => DimensionScore::absent(
            "oo_coupling",
            "Avg coupling-between-objects (no symbol data)",
        ),
    }
}

/// DSM propagation cost: average fraction of the import graph reachable from a
/// node. Bounded (skipped → absent) above 2000 nodes to avoid O(N·E) blowup.
async fn propagation_cost_dim(pool: &PgPool, project_id: i32) -> DimensionScore {
    #[derive(sqlx::FromRow)]
    struct Edge {
        source_file_id: i64,
        target_file_id: i64,
    }
    let edges: Vec<Edge> = sqlx::query_as::<_, Edge>(
        "SELECT source_file_id, target_file_id FROM code_graph_edges
         WHERE project_id = $1 AND edge_type = 'import' AND target_file_id IS NOT NULL
           AND target_project_id IS NULL",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    if edges.is_empty() {
        return DimensionScore::absent(
            "propagation_cost",
            "DSM propagation cost (no import graph)",
        );
    }

    // Index nodes.
    use std::collections::HashMap;
    let mut idx: HashMap<i64, usize> = HashMap::new();
    for e in &edges {
        let n = idx.len();
        idx.entry(e.source_file_id).or_insert(n);
        let n = idx.len();
        idx.entry(e.target_file_id).or_insert(n);
    }
    let n = idx.len();
    if n == 0 || n > 2000 {
        return DimensionScore::absent(
            "propagation_cost",
            "DSM propagation cost (graph too large to bound)",
        );
    }
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for e in &edges {
        let (s, t) = (idx[&e.source_file_id], idx[&e.target_file_id]);
        adj[s].push(t);
    }

    // Sum reachable-set sizes via BFS from each node.
    let mut total_reachable: u64 = 0;
    let mut visited = vec![u32::MAX; n];
    for start in 0..n {
        let mut stack = vec![start];
        visited[start] = start as u32;
        let mut count = 0u64;
        while let Some(u) = stack.pop() {
            count += 1;
            for &v in &adj[u] {
                if visited[v] != start as u32 {
                    visited[v] = start as u32;
                    stack.push(v);
                }
            }
        }
        total_reachable += count;
    }
    let propagation_cost = total_reachable as f64 / (n as f64 * n as f64);
    DimensionScore::present(
        "propagation_cost",
        "DSM propagation cost (avg reachable fraction)",
        100.0 * (1.0 - propagation_cost.clamp(0.0, 1.0)),
    )
}

/// Security dimensions derived from the collected findings + advisory presence.
async fn security_dims(
    pool: &PgPool,
    findings: &[Finding],
    file_count: i64,
) -> Vec<DimensionScore> {
    let files = file_count.max(1) as f64;
    let count_of = |tool: &str| findings.iter().filter(|f| f.source_tool == tool).count() as f64;

    let secret_weighted: f64 = findings
        .iter()
        .filter(|f| f.source_tool == "secret_detection")
        .map(|f| {
            if f.severity == Severity::Critical {
                3.0
            } else {
                1.0
            }
        })
        .sum();
    let secret_hygiene = 100.0 * (1.0 - (secret_weighted / files).clamp(0.0, 1.0));

    let injection_sites = count_of("injection_candidates") + count_of("taint_analysis");
    let injection_risk = 100.0 * (1.0 - (injection_sites / files * 10.0).clamp(0.0, 1.0));

    let crypto = count_of("crypto_misuse") + count_of("unsafe_deserialization");
    let crypto_hygiene = 100.0 * (1.0 - (crypto / files * 10.0).clamp(0.0, 1.0));

    // supply_chain is data-absent unless OSV advisories were imported.
    let advisories: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vuln_advisories")
        .fetch_one(pool)
        .await
        .unwrap_or(0);
    let supply_chain = if advisories == 0 {
        DimensionScore::absent("supply_chain", "Dependency advisories (none imported)")
    } else {
        let weighted: f64 = findings
            .iter()
            .filter(|f| f.source_tool == "cve_supply_chain")
            .map(|f| f.severity.weight())
            .sum();
        DimensionScore::present(
            "supply_chain",
            "Known-vulnerable dependency exposure",
            100.0 * (1.0 - (weighted / 20.0).clamp(0.0, 1.0)),
        )
    };

    vec![
        DimensionScore::present(
            "secret_hygiene",
            "Absence of hardcoded secrets",
            secret_hygiene,
        ),
        DimensionScore::present(
            "injection_risk",
            "Absence of injection/taint sites",
            injection_risk,
        ),
        DimensionScore::present(
            "crypto_hygiene",
            "Sound cryptography & deserialization",
            crypto_hygiene,
        ),
        supply_chain,
    ]
}

fn security_dims_skipped() -> Vec<DimensionScore> {
    vec![
        DimensionScore::absent(
            "secret_hygiene",
            "Absence of hardcoded secrets - N/A (finding collectors skipped)",
        ),
        DimensionScore::absent(
            "injection_risk",
            "Absence of injection/taint sites - N/A (finding collectors skipped)",
        ),
        DimensionScore::absent(
            "crypto_hygiene",
            "Sound cryptography & deserialization - N/A (finding collectors skipped)",
        ),
        DimensionScore::absent(
            "supply_chain",
            "Dependency advisories - N/A (finding collectors skipped)",
        ),
    ]
}

async fn effect_breakdown_value(pool: &PgPool, project_id: i32) -> serde_json::Value {
    let map = crate::mcp::tools::sema_helpers::effects::effect_counts(pool, project_id)
        .await
        .unwrap_or_default();
    serde_json::to_value(map).unwrap_or(serde_json::Value::Null)
}

/// Time + bound a single collector future.
async fn timed<'a>(
    name: &'static str,
    cat: FindingCategory,
    fut: CFut<'a>,
    secs: u64,
) -> (
    &'static str,
    FindingCategory,
    u64,
    Result<Vec<Finding>, String>,
) {
    let start = Instant::now();
    let r = tokio::time::timeout(Duration::from_secs(secs), fut).await;
    let millis = start.elapsed().as_millis() as u64;
    let res = match r {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("{e:?}")),
        Err(_) => Err(format!("timed out after {secs}s")),
    };
    (name, cat, millis, res)
}

/// Fan out all finding collectors and gather (appendix rows, findings).
async fn run_collectors(
    ctx: &SystemContext,
    pid: i32,
    name: &str,
    secs: u64,
) -> (Vec<ToolRun>, Vec<Finding>) {
    use FindingCategory::*;

    // Table-backed (fast) collectors.
    let light: Vec<(&'static str, FindingCategory, CFut)> = vec![
        (
            "complexity_hotspots",
            CodeHealth,
            Box::pin(ch_c::collect_complexity_hotspots(ctx, pid, name)),
        ),
        (
            "bug_prediction",
            CodeHealth,
            Box::pin(ch_c::collect_bug_prediction(ctx, pid, name)),
        ),
        (
            "refactoring_report",
            CodeHealth,
            Box::pin(ch_c::collect_refactoring_report(ctx, pid, name)),
        ),
        (
            "signature_lint",
            CodeHealth,
            Box::pin(ch_c::collect_signature_lint(ctx, pid, name)),
        ),
        (
            "circular_dependencies",
            Architecture,
            Box::pin(arch_c::collect_circular_dependencies(ctx, pid, name)),
        ),
        (
            "architecture_violations",
            Architecture,
            Box::pin(arch_c::collect_architecture_violations(ctx, pid, name)),
        ),
        (
            "design_smell_detection",
            Architecture,
            Box::pin(arch_c::collect_design_smell_detection(ctx, pid, name)),
        ),
        (
            "coupling_cohesion_report",
            Architecture,
            Box::pin(arch_c::collect_coupling_cohesion_report(ctx, pid, name)),
        ),
        (
            "feature_envy",
            Architecture,
            Box::pin(arch_c::collect_feature_envy(ctx, pid, name)),
        ),
        (
            "shotgun_surgery",
            Architecture,
            Box::pin(arch_c::collect_shotgun_surgery(ctx, pid, name)),
        ),
        (
            "lcom4",
            Architecture,
            Box::pin(arch_c::collect_lcom4(ctx, pid, name)),
        ),
        (
            "find_misplaced_code",
            Architecture,
            Box::pin(arch_c::collect_find_misplaced_code(ctx, pid, name)),
        ),
        (
            "panic_paths",
            Concurrency,
            Box::pin(cc_c::collect_panic_paths(ctx, pid, name)),
        ),
        (
            "unsafe_clusters",
            Concurrency,
            Box::pin(cc_c::collect_unsafe_clusters(ctx, pid, name)),
        ),
        (
            "dependency_health",
            Dependency,
            Box::pin(dep_c::collect_dependency_health(ctx, pid, name)),
        ),
        (
            "find_duplicates",
            Duplication,
            Box::pin(dup_c::collect_find_duplicates(ctx, pid, name)),
        ),
        (
            "clone_density",
            Duplication,
            Box::pin(dup_c::collect_clone_density(ctx, pid, name)),
        ),
        (
            "find_orphans",
            Hygiene,
            Box::pin(hy_c::collect_find_orphans(ctx, pid, name)),
        ),
        (
            "dead_code_reachability",
            Hygiene,
            Box::pin(hy_c::collect_dead_code_reachability(ctx, pid, name)),
        ),
        (
            "stale_zombie",
            Hygiene,
            Box::pin(hy_c::collect_stale_zombie(ctx, pid, name)),
        ),
        (
            "anomaly_detection",
            Hygiene,
            Box::pin(hy_c::collect_anomaly_detection(ctx, pid, name)),
        ),
        (
            "naming_consistency",
            Hygiene,
            Box::pin(hy_c::collect_naming_consistency(ctx, pid, name)),
        ),
        (
            "import_hygiene",
            Hygiene,
            Box::pin(hy_c::collect_import_hygiene(ctx, pid, name)),
        ),
        (
            "test_coverage_gaps",
            TestsDocs,
            Box::pin(td_c::collect_test_coverage_gaps(ctx, pid, name)),
        ),
        (
            "doc_coverage_gaps",
            TestsDocs,
            Box::pin(td_c::collect_doc_coverage_gaps(ctx, pid, name)),
        ),
        (
            "test_smells",
            TestsDocs,
            Box::pin(td_c::collect_test_smells(ctx, pid, name)),
        ),
        (
            "mutation_score_surrogate",
            TestsDocs,
            Box::pin(td_c::collect_mutation_score_surrogate(ctx, pid, name)),
        ),
        (
            "cve_supply_chain",
            Security,
            Box::pin(sec_c::collect_cve_supply_chain(ctx, pid, name)),
        ),
    ];

    // Full-file content scanners (memory-bounded wave).
    let heavy: Vec<(&'static str, FindingCategory, CFut)> = vec![
        (
            "technical_debt_analysis",
            CodeHealth,
            Box::pin(ch_c::collect_technical_debt(ctx, pid, name)),
        ),
        (
            "documented_tech_debt",
            CodeHealth,
            Box::pin(ch_c::collect_documented_tech_debt(ctx, pid, name)),
        ),
        (
            "secret_detection",
            Security,
            Box::pin(sec_c::collect_secret_detection(ctx, pid, name)),
        ),
        (
            "injection_candidates",
            Security,
            Box::pin(sec_c::collect_injection_candidates(ctx, pid, name)),
        ),
        (
            "crypto_misuse",
            Security,
            Box::pin(sec_c::collect_crypto_misuse(ctx, pid, name)),
        ),
        (
            "unsafe_deserialization",
            Security,
            Box::pin(sec_c::collect_unsafe_deserialization(ctx, pid, name)),
        ),
        (
            "pii_spread",
            Security,
            Box::pin(sec_c::collect_pii_spread(ctx, pid, name)),
        ),
        (
            "taint_analysis",
            Security,
            Box::pin(sec_c::collect_taint_analysis(ctx, pid, name)),
        ),
        (
            "unprotected_routes",
            Security,
            Box::pin(sec_c::collect_unprotected_routes(ctx, pid, name)),
        ),
        (
            "attack_vulnerability",
            Security,
            Box::pin(sec_c::collect_attack_vulnerability(ctx, pid, name)),
        ),
        (
            "blocking_in_async",
            Concurrency,
            Box::pin(cc_c::collect_blocking_in_async(ctx, pid, name)),
        ),
        (
            "lockset_races",
            Concurrency,
            Box::pin(cc_c::collect_lockset_races(ctx, pid, name)),
        ),
        (
            "send_sync_violations",
            Concurrency,
            Box::pin(cc_c::collect_send_sync_violations(ctx, pid, name)),
        ),
        (
            "deadlock_candidates",
            Concurrency,
            Box::pin(cc_c::collect_deadlock_candidates(ctx, pid, name)),
        ),
        (
            "deprecated_but_used",
            Dependency,
            Box::pin(dep_c::collect_deprecated_but_used(ctx, pid, name)),
        ),
        (
            "flaky_test_candidates",
            TestsDocs,
            Box::pin(td_c::collect_flaky_test_candidates(ctx, pid, name)),
        ),
        (
            "doc_code_drift",
            TestsDocs,
            Box::pin(td_c::collect_doc_code_drift(ctx, pid, name)),
        ),
    ];

    // Build the future vectors with explicit loops rather than `.map(closure)`
    // to sidestep a higher-ranked-lifetime inference failure on the async fn.
    let mut light_futs = Vec::with_capacity(light.len());
    for (n, c, f) in light {
        light_futs.push(timed(n, c, f, secs));
    }
    let light_results = futures::future::join_all(light_futs).await;

    let mut heavy_futs = Vec::with_capacity(heavy.len());
    for (n, c, f) in heavy {
        heavy_futs.push(timed(n, c, f, secs));
    }
    let heavy_results = stream::iter(heavy_futs)
        .buffer_unordered(4)
        .collect::<Vec<_>>()
        .await;

    let mut tool_runs = Vec::new();
    let mut findings = Vec::new();
    for (tool, category, millis, res) in light_results.into_iter().chain(heavy_results) {
        match res {
            Ok(mut v) => {
                let note = if v.is_empty() {
                    Some("no findings".to_string())
                } else {
                    None
                };
                tool_runs.push(ToolRun {
                    tool: tool.to_string(),
                    category,
                    finding_count: v.len(),
                    millis,
                    outcome: ToolOutcome::Ran,
                    note,
                });
                findings.append(&mut v);
            }
            Err(msg) => {
                tool_runs.push(ToolRun {
                    tool: tool.to_string(),
                    category,
                    finding_count: 0,
                    millis,
                    outcome: ToolOutcome::ErroredOrTimedOut,
                    note: Some(msg.clone()),
                });
                findings.push(
                    Finding::new(
                        tool,
                        category,
                        name,
                        Severity::Info,
                        format!("{tool} could not run: {msg}"),
                    )
                    .with_kind("tool_unavailable"),
                );
            }
        }
    }
    tool_runs.sort_by(|a, b| a.tool.cmp(&b.tool));
    (tool_runs, findings)
}
