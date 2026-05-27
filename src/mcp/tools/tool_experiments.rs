//! MCP tool bodies for the scientific-experiment subsystem.
//!
//! The server is the methodology authority: `experiment_open` /
//! `experiment_protocol` prescribe the design (sample size, test, frozen
//! acceptance criterion, reproducibility checklist); `experiment_record_measurement`
//! validates submitted raw samples for conformance; `experiment_decide` runs
//! the frozen test and renders the verdict (mirroring it into the memory
//! graph). `experiment_search`/`_get`/`_list`/`_timeline` expose the record
//! cross-project. The agent executes the work; the daemon never spawns
//! arbitrary commands.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use pgvector::Vector;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use sha2::{Digest, Sha256};

use crate::context::SystemContext;
use crate::db::queries::{self, InsertExperimentResult};
use crate::experiment::{extract, ledger, mirror, protocol};
use crate::mcp::server::{
    ExperimentDecideParams, ExperimentGetParams, ExperimentListParams, ExperimentLogArtifactParams,
    ExperimentOpenParams, ExperimentProtocolParams, ExperimentRecordMeasurementParams,
    ExperimentRenderLedgerParams, ExperimentSearchParams, ExperimentTimelineParams,
};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::stats::acceptance::{self, AcceptanceCriterion};
use crate::stats::inference::{self, Correction, TestResult};

const VALID_KINDS: &[&str] = &[
    "optimization",
    "feature_refactor",
    "feature_addition",
    "bugfix",
    "investigation",
    "other",
];
const VALID_ARM_KINDS: &[&str] = &["control", "treatment", "baseline"];

/// Embed `text` when `on`, mapping failures to `None` (the migration cron
/// backfills NULL embeddings, so a transient embed failure is not fatal).
async fn embed_opt(ctx: &SystemContext, on: bool, text: &str) -> Option<Vector> {
    if !on || text.trim().is_empty() {
        return None;
    }
    match ctx.embed().embed_query(text).await {
        Ok(v) => Some(Vector::from(v)),
        Err(e) => {
            tracing::warn!(error = %e, "experiment embed-on-write failed; leaving NULL for cron backfill");
            None
        }
    }
}

/// Parse a correction-method string to the inference enum (default BH).
fn parse_correction(s: &str) -> Correction {
    match s {
        "bonferroni" => Correction::Bonferroni,
        "none" => Correction::None,
        _ => Correction::BenjaminiHochberg,
    }
}

/// Derive a kebab-case slug from a title.
fn slugify(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut prev_dash = false;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            out.extend(ch.to_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let s = out.trim_matches('-').to_string();
    if s.is_empty() {
        "experiment".to_string()
    } else {
        s
    }
}

/// The serde snake_case name of a `TestKind` (e.g. "welch_t").
fn test_kind_str(tr: &TestResult) -> String {
    serde_json::to_value(tr.kind)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| "unknown".to_string())
}

fn effect_kind_str(tr: &TestResult) -> Option<String> {
    tr.effect_kind
        .and_then(|k| serde_json::to_value(k).ok())
        .and_then(|v| v.as_str().map(String::from))
}

/// NaN → None (non-NHST evidence carries NaN; store SQL NULL instead).
fn finite_or_none(x: f64) -> Option<f64> {
    if x.is_finite() { Some(x) } else { None }
}

// ============================================================================
// experiment_open
// ============================================================================

pub async fn tool_experiment_open(
    ctx: &SystemContext,
    params: ExperimentOpenParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    if params.title.trim().is_empty()
        || params.question.trim().is_empty()
        || params.hypothesis.trim().is_empty()
        || params.primary_metric.trim().is_empty()
    {
        return Err(McpError::invalid_params(
            "title, question, hypothesis, and primary_metric must be non-empty",
            None,
        ));
    }
    let kind = params.kind.as_deref().unwrap_or("other");
    if !VALID_KINDS.contains(&kind) {
        return Err(McpError::invalid_params(
            format!("unknown kind '{kind}'; expected one of {VALID_KINDS:?}"),
            None,
        ));
    }
    let predicted_direction = params.predicted_direction.as_deref().unwrap_or("either");

    // Resolve the acceptance criterion: supplied JSON, or a kind-appropriate
    // default (Welch p<0.05 ∧ |d|≥0.5 ∧ correct direction).
    let criterion: AcceptanceCriterion = match &params.acceptance_criterion {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
            McpError::invalid_params(format!("invalid acceptance_criterion: {e}"), None)
        })?,
        None => AcceptanceCriterion::default_optimization(params.lower_is_better.unwrap_or(true)),
    };
    let criterion_json = serde_json::to_string(&criterion)
        .map_err(|e| McpError::internal_error(format!("serialize criterion: {e}"), None))?;

    let cfg = ctx.config().load();
    let exp_cfg = &cfg.experiments;

    // Prescribe the protocol (kind-aware; sizes the sample via power analysis).
    let proto = protocol::prescribe(
        kind,
        &params.primary_metric,
        params.unit.as_deref(),
        predicted_direction,
        &criterion,
        exp_cfg,
        params.expected_effect,
    );
    let planned_n = proto.required_samples_per_arm.map(|n| n as i32);
    let embed_on_write = exp_cfg.embed_on_write;
    let correction = exp_cfg.default_correction.clone();
    drop(cfg);

    let slug = params
        .slug
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| slugify(&params.title));
    let hardware_json = params
        .hardware
        .as_ref()
        .map(|v| v.to_string())
        .unwrap_or_else(|| "{}".to_string());

    // Embeddings (synchronous on write; cron backfills on failure).
    let exp_text = format!(
        "{} {} {}",
        params.title,
        params.question,
        params.context.as_deref().unwrap_or("")
    );
    let exp_embedding = embed_opt(ctx, embed_on_write, &exp_text).await;
    let hyp_embedding = embed_opt(ctx, embed_on_write, &params.hypothesis).await;

    let experiment_id = queries::insert_experiment(
        pool,
        &slug,
        &params.title,
        &params.question,
        params.context.as_deref(),
        kind,
        params.project_id,
        &hardware_json,
        params.git_ref.as_deref(),
        params.plan_ref.as_deref(),
        &correction,
        exp_embedding,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("insert_experiment: {e}"), None))?;

    let hypothesis_id = queries::insert_experiment_hypothesis(
        pool,
        experiment_id,
        &params.hypothesis,
        &params.primary_metric,
        params.unit.as_deref(),
        predicted_direction,
        &criterion_json,
        planned_n,
        hyp_embedding,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("insert_experiment_hypothesis: {e}"), None))?;

    // Code anchors (best-effort path resolution).
    let mut anchored = 0usize;
    if let Some(paths) = &params.anchor_paths {
        for path in paths {
            if let Ok(Some(file_id)) =
                queries::resolve_experiment_file_id(pool, params.project_id, path).await
                && queries::insert_experiment_code_anchor(
                    pool,
                    experiment_id,
                    Some(file_id),
                    None,
                    None,
                    "concerns",
                )
                .await
                .is_ok()
            {
                anchored += 1;
            }
        }
    }

    // Mirror into the memory graph (best-effort).
    if let Err(e) = mirror::mirror_open(
        pool,
        &slug,
        &params.title,
        &params.question,
        kind,
        params.project_id,
        hypothesis_id,
        &params.hypothesis,
        &params.primary_metric,
    )
    .await
    {
        tracing::warn!(error = %e, "experiment mirror_open failed (non-fatal)");
    }

    ctx.stats()
        .experiments_opened
        .fetch_add(1, Ordering::Relaxed);

    json_result(&json!({
        "experiment_id": experiment_id,
        "hypothesis_id": hypothesis_id,
        "slug": slug,
        "kind": kind,
        "criterion_locked": true,
        "anchored_files": anchored,
        "protocol": proto,
    }))
}

// ============================================================================
// experiment_protocol
// ============================================================================

pub async fn tool_experiment_protocol(
    ctx: &SystemContext,
    params: ExperimentProtocolParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let core = queries::get_experiment_core(pool, params.experiment_id, params.slug.as_deref())
        .await
        .map_err(|e| McpError::internal_error(format!("get_experiment_core: {e}"), None))?
        .ok_or_else(|| McpError::invalid_params("experiment not found", None))?;

    // Pick the hypothesis (explicit id, else the experiment's first).
    let hyp = match params.hypothesis_id {
        Some(id) => queries::get_experiment_hypothesis(pool, id)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("get_experiment_hypothesis: {e}"), None)
            })?,
        None => queries::list_experiment_hypotheses(pool, core.id)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("list_experiment_hypotheses: {e}"), None)
            })?
            .into_iter()
            .next(),
    }
    .ok_or_else(|| McpError::invalid_params("no hypothesis found for experiment", None))?;

    let criterion: AcceptanceCriterion = serde_json::from_str(&hyp.acceptance_criterion_json)
        .map_err(|e| McpError::internal_error(format!("stored criterion parse: {e}"), None))?;

    let cfg = ctx.config().load();
    let proto = protocol::prescribe(
        &core.kind,
        &hyp.primary_metric,
        hyp.unit.as_deref(),
        &hyp.predicted_direction,
        &criterion,
        &cfg.experiments,
        params.expected_effect,
    );
    drop(cfg);

    json_result(&json!({
        "experiment_id": core.id,
        "slug": core.slug,
        "hypothesis_id": hyp.id,
        "protocol": proto,
    }))
}

// ============================================================================
// experiment_record_measurement
// ============================================================================

pub async fn tool_experiment_record_measurement(
    ctx: &SystemContext,
    params: ExperimentRecordMeasurementParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    if params.samples.is_empty() {
        return Err(McpError::invalid_params("samples must be non-empty", None));
    }
    if params.samples.iter().any(|v| !v.is_finite()) {
        return Err(McpError::invalid_params("samples must all be finite", None));
    }
    if !VALID_ARM_KINDS.contains(&params.arm_kind.as_str()) {
        return Err(McpError::invalid_params(
            format!("arm_kind must be one of {VALID_ARM_KINDS:?}"),
            None,
        ));
    }
    if let Some(keys) = &params.unit_keys
        && keys.len() != params.samples.len()
    {
        return Err(McpError::invalid_params(
            "unit_keys length must equal samples length",
            None,
        ));
    }

    let command_spec = params
        .command_spec
        .as_ref()
        .map(|v| v.to_string())
        .unwrap_or_else(|| "{}".to_string());
    let run_plan = params
        .run_plan
        .as_ref()
        .map(|v| v.to_string())
        .unwrap_or_else(|| "{}".to_string());
    let host_meta = params
        .host_meta
        .as_ref()
        .map(|v| v.to_string())
        .unwrap_or_else(|| "{}".to_string());

    let run_id = queries::upsert_experiment_run(
        pool,
        params.experiment_id,
        params.hypothesis_id,
        &params.arm_label,
        &params.arm_kind,
        &command_spec,
        &run_plan,
        &host_meta,
        params.git_ref.as_deref(),
        params.source.as_deref(),
        params.seed.unwrap_or(0),
    )
    .await
    .map_err(|e| McpError::internal_error(format!("upsert_experiment_run: {e}"), None))?;

    let is_warmup = params.is_warmup.unwrap_or(false);
    let inserted = queries::insert_experiment_samples(
        pool,
        run_id,
        &params.arm_label,
        &params.metric,
        &params.samples,
        params.unit_keys.as_deref(),
        is_warmup,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("insert_experiment_samples: {e}"), None))?;

    queries::set_experiment_status(pool, params.experiment_id, "measuring")
        .await
        .map_err(|e| McpError::internal_error(format!("set_experiment_status: {e}"), None))?;

    // Summary over the just-submitted (non-warm-up) samples.
    let summary = inference::summarize(&params.samples);

    // Conformance check: compare the total non-warm-up samples recorded so far
    // for this arm/metric against the protocol's required_samples_per_arm.
    let mut conformance = json!({ "checked": false });
    if !is_warmup
        && let Some(hyp_id) = params.hypothesis_id
        && let Ok(Some(hyp)) = queries::get_experiment_hypothesis(pool, hyp_id).await
        && let Ok(criterion) =
            serde_json::from_str::<AcceptanceCriterion>(&hyp.acceptance_criterion_json)
        && let Ok(Some(core)) =
            queries::get_experiment_core(pool, Some(params.experiment_id), None).await
    {
        let cfg = ctx.config().load();
        let proto = protocol::prescribe(
            &core.kind,
            &hyp.primary_metric,
            hyp.unit.as_deref(),
            &hyp.predicted_direction,
            &criterion,
            &cfg.experiments,
            None,
        );
        drop(cfg);
        let total = queries::load_experiment_samples(
            pool,
            params.experiment_id,
            params.hypothesis_id,
            &params.arm_label,
            &params.metric,
        )
        .await
        .map(|v| v.len())
        .unwrap_or(0);
        let required = proto.required_samples_per_arm;
        let met = required.map(|r| total as u32 >= r).unwrap_or(true);
        // metric/unit-match conformance: a submitted unit that conflicts with
        // the hypothesis's declared unit is flagged — samples in the wrong unit
        // would silently corrupt the test.
        let unit_match = match (params.unit.as_deref(), hyp.unit.as_deref()) {
            (Some(submitted), Some(expected)) => submitted == expected,
            _ => true,
        };
        let unit_warning = (!unit_match).then(|| {
            format!(
                "submitted unit {:?} does not match the hypothesis's declared unit {:?}",
                params.unit, hyp.unit
            )
        });
        conformance = json!({
            "checked": true,
            "metric_nature": proto.metric_nature,
            "required_samples_per_arm": required,
            "recorded_samples_this_arm": total,
            "met": met,
            "unit_match": unit_match,
            "warning": if met { serde_json::Value::Null } else {
                json!(format!("only {total} non-warm-up samples for arm '{}'; protocol prescribes >= {:?}", params.arm_label, required))
            },
            "unit_warning": unit_warning,
        });
    }

    ctx.stats()
        .experiment_measurements_recorded
        .fetch_add(1, Ordering::Relaxed);

    json_result(&json!({
        "run_id": run_id,
        "inserted_samples": inserted,
        "is_warmup": is_warmup,
        "summary": {
            "n": summary.n,
            "mean": summary.mean,
            "std_dev": summary.std_dev,
            "median": summary.median,
            "min": summary.min,
            "max": summary.max,
        },
        "conformance": conformance,
    }))
}

// ============================================================================
// experiment_decide
// ============================================================================

pub async fn tool_experiment_decide(
    ctx: &SystemContext,
    params: ExperimentDecideParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let hyp = queries::get_experiment_hypothesis(pool, params.hypothesis_id)
        .await
        .map_err(|e| McpError::internal_error(format!("get_experiment_hypothesis: {e}"), None))?
        .ok_or_else(|| McpError::invalid_params("hypothesis not found", None))?;
    let core = queries::get_experiment_core(pool, Some(hyp.experiment_id), None)
        .await
        .map_err(|e| McpError::internal_error(format!("get_experiment_core: {e}"), None))?
        .ok_or_else(|| McpError::invalid_params("experiment not found", None))?;

    let criterion: AcceptanceCriterion = serde_json::from_str(&hyp.acceptance_criterion_json)
        .map_err(|e| McpError::internal_error(format!("stored criterion parse: {e}"), None))?;
    let metric = params
        .metric
        .clone()
        .unwrap_or_else(|| hyp.primary_metric.clone());
    let control_arm = params.control_arm.as_deref().unwrap_or("control");
    let treatment_arm = params.treatment_arm.as_deref().unwrap_or("treatment");

    // Anti-p-hacking guard: the criterion must predate the first measurement.
    if let Ok(Some(first)) = queries::earliest_measurement_time(pool, params.hypothesis_id).await
        && hyp.criterion_locked_at > first
    {
        return Err(McpError::invalid_params(
            "acceptance criterion was locked AFTER measurements began (pre-registration violated)",
            None,
        ));
    }

    // Load samples (control may be empty for single-arm / observational criteria).
    let control: Vec<f64> = queries::load_experiment_samples(
        pool,
        hyp.experiment_id,
        Some(params.hypothesis_id),
        control_arm,
        &metric,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("load control samples: {e}"), None))?
    .into_iter()
    .map(|(v, _)| v)
    .collect();
    let treatment: Vec<f64> = queries::load_experiment_samples(
        pool,
        hyp.experiment_id,
        Some(params.hypothesis_id),
        treatment_arm,
        &metric,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("load treatment samples: {e}"), None))?
    .into_iter()
    .map(|(v, _)| v)
    .collect();

    // Robustness cross-check (advisory; the pre-registered criterion still
    // decides): assess normality and report a parallel non-parametric result
    // so a Welch verdict on non-normal data is flagged, not silently trusted.
    let mut robustness = serde_json::Value::Null;
    if control.len() >= 2 && treatment.len() >= 2 {
        let (rec, warnings) = inference::recommend_two_sample_test(&control, &treatment);
        let mwu = inference::mann_whitney_u(&control, &treatment, inference::Tail::TwoSided)
            .ok()
            .map(|r| (finite_or_none(r.p_value), r.effect_size));
        robustness = json!({
            "recommended_test": format!("{rec:?}"),
            "mann_whitney_p": mwu.as_ref().and_then(|(p, _)| *p),
            "cliffs_delta": mwu.as_ref().and_then(|(_, e)| *e),
            "warnings": warnings,
        });
    }

    let correction = parse_correction(&core.correction);

    // Evaluate. A StatsError (e.g. too few samples) → inconclusive, not reject.
    let (verdict, accepted, decision_opt) =
        match acceptance::evaluate(&criterion, &control, &treatment, correction) {
            Ok(d) => {
                let v = if d.accepted { "accepted" } else { "rejected" };
                (v.to_string(), d.accepted, Some(d))
            }
            Err(e) => {
                tracing::info!(error = %e, "experiment_decide: inconclusive (test could not run)");
                ("inconclusive".to_string(), false, None)
            }
        };

    // Headline numbers from the first evidence row (if any).
    let headline = decision_opt.as_ref().and_then(|d| d.evidence.first());
    let test_type = headline
        .map(test_kind_str)
        .unwrap_or_else(|| "none".to_string());
    let statistic = headline.and_then(|t| finite_or_none(t.statistic));
    let df = headline.and_then(|t| t.df).and_then(finite_or_none);
    let p_value = headline.and_then(|t| finite_or_none(t.p_value));
    let effect_size = headline
        .and_then(|t| t.effect_size)
        .and_then(finite_or_none);
    let effect_size_kind = headline.and_then(effect_kind_str);
    let ci_low = headline.and_then(|t| t.ci_low).and_then(finite_or_none);
    let ci_high = headline.and_then(|t| t.ci_high).and_then(finite_or_none);
    let ci_level = headline.map(|t| t.ci_level);

    let rationale = {
        let base = decision_opt
            .as_ref()
            .map(|d| d.rationale.clone())
            .unwrap_or_else(|| {
                "Inconclusive: the statistical test could not be run on the recorded samples."
                    .to_string()
            });
        match &params.rationale_note {
            Some(note) if !note.trim().is_empty() => format!("{base}\n\nOperator note: {note}"),
            _ => base,
        }
    };
    let test_result_json = decision_opt
        .as_ref()
        .map(|d| serde_json::to_string(&d.evidence).unwrap_or_else(|_| "[]".to_string()))
        .unwrap_or_else(|| "[]".to_string());

    let cfg = ctx.config().load();
    let embed_on_write = cfg.experiments.embed_on_write;
    drop(cfg);
    let decision_embedding = embed_opt(
        ctx,
        embed_on_write,
        &format!("{verdict} {metric}: {rationale}"),
    )
    .await;

    let control_run_id = queries::find_experiment_run_id(
        pool,
        hyp.experiment_id,
        Some(params.hypothesis_id),
        control_arm,
    )
    .await
    .ok()
    .flatten();
    let treatment_run_id = queries::find_experiment_run_id(
        pool,
        hyp.experiment_id,
        Some(params.hypothesis_id),
        treatment_arm,
    )
    .await
    .ok()
    .flatten();

    // Mirror the decision into the memory graph (best-effort) → observation id.
    let summary = format!(
        "For {metric} in experiment '{}': {} (test={test_type}, p={:?}, effect={:?})",
        core.slug, verdict, p_value, effect_size
    );
    let observation_id = mirror::mirror_decision(
        pool,
        &core.slug,
        None,
        params.hypothesis_id,
        &metric,
        &verdict,
        &summary,
    )
    .await
    .unwrap_or(None);

    let criterion_snapshot = serde_json::to_string(&criterion).unwrap_or_else(|_| "{}".to_string());
    let result_id = queries::insert_experiment_result(
        pool,
        InsertExperimentResult {
            experiment_id: hyp.experiment_id,
            hypothesis_id: params.hypothesis_id,
            test_type: &test_type,
            metric_name: &metric,
            control_run_id,
            treatment_run_id,
            statistic,
            df,
            p_value,
            effect_size,
            effect_size_kind: effect_size_kind.as_deref(),
            ci_low,
            ci_high,
            ci_level,
            verdict: &verdict,
            accepted,
            correction: Some(&core.correction),
            criterion_snapshot_json: &criterion_snapshot,
            test_result_json: &test_result_json,
            rationale: Some(&rationale),
            decided_by: params.decided_by.as_deref(),
            embedding: decision_embedding,
            observation_id,
        },
    )
    .await
    .map_err(|e| McpError::internal_error(format!("insert_experiment_result: {e}"), None))?;

    if let Some(oid) = observation_id {
        let _ = queries::set_result_observation_id(pool, result_id, oid).await;
        let _ = queries::set_experiment_observation_id(pool, hyp.experiment_id, oid).await;
    }
    queries::set_hypothesis_verdict(pool, params.hypothesis_id, &verdict)
        .await
        .map_err(|e| McpError::internal_error(format!("set_hypothesis_verdict: {e}"), None))?;
    queries::set_experiment_status(pool, hyp.experiment_id, "decided")
        .await
        .map_err(|e| McpError::internal_error(format!("set_experiment_status: {e}"), None))?;

    // Optional: graduate a confirmed/rejected verdict into the cross-agent
    // best-practice ledger (consensus → durable-mandate pipeline).
    let link = params.link_outcome.unwrap_or(true);
    let mut linked_outcome_id: Option<i64> = None;
    if link && matches!(verdict.as_str(), "accepted" | "rejected") {
        use crate::a2a::best_practices::{Outcome, OutcomeReport, record_outcome};
        let outcome = if verdict == "accepted" {
            Outcome::Worked
        } else {
            Outcome::Failed
        };
        let report = OutcomeReport {
            agent_id: params
                .decided_by
                .clone()
                .unwrap_or_else(|| "experiment".to_string()),
            project_id: None,
            task_kind: format!("experiment:{}", core.slug),
            approach: hyp.statement.clone(),
            outcome,
            confidence: 0.8,
            evidence: Some(rationale.clone()),
            parent_task_id: None,
            tier: "procedural",
        };
        match record_outcome(pool, &report).await {
            Ok(r) => linked_outcome_id = Some(r.outcome_id),
            Err(e) => tracing::warn!(error = %e, "experiment_decide: agent_outcomes link failed"),
        }
    }

    ctx.stats()
        .experiment_decisions_made
        .fetch_add(1, Ordering::Relaxed);

    json_result(&json!({
        "result_id": result_id,
        "experiment_id": hyp.experiment_id,
        "hypothesis_id": params.hypothesis_id,
        "verdict": verdict,
        "accepted": accepted,
        "test_type": test_type,
        "statistic": statistic,
        "p_value": p_value,
        "effect_size": effect_size,
        "effect_size_kind": effect_size_kind,
        "ci_low": ci_low,
        "ci_high": ci_high,
        "n_control": control.len(),
        "n_treatment": treatment.len(),
        "rationale": rationale,
        "observation_id": observation_id,
        "linked_outcome_id": linked_outcome_id,
        "robustness": robustness,
    }))
}

// ============================================================================
// experiment_search
// ============================================================================

pub async fn tool_experiment_search(
    ctx: &SystemContext,
    params: ExperimentSearchParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .experiment_searches
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    if params.query.trim().is_empty() {
        return Err(McpError::invalid_params("query must be non-empty", None));
    }
    let limit = params.limit.unwrap_or(20).clamp(1, 100) as i64;

    // Prefer vector search over the experiment embeddings; fall back to FTS.
    let hits = match ctx.embed().embed_query(&params.query).await {
        Ok(v) => {
            let qvec = Vector::from(v);
            queries::experiment_search_vector(
                pool,
                &qvec,
                params.project_id,
                params.kind.as_deref(),
                params.verdict.as_deref(),
                limit,
            )
            .await
        }
        Err(_) => {
            queries::experiment_search_fts(
                pool,
                &params.query,
                params.project_id,
                params.kind.as_deref(),
                limit,
            )
            .await
        }
    }
    .map_err(|e| McpError::internal_error(format!("experiment_search: {e}"), None))?;

    let results: Vec<_> = hits
        .into_iter()
        .map(|h| {
            json!({
                "experiment_id": h.id,
                "slug": h.slug,
                "title": h.title,
                "kind": h.kind,
                "status": h.status,
                "project": h.project,
                "similarity": h.similarity,
                "verdict": h.verdict,
                "p_value": h.p_value,
            })
        })
        .collect();
    json_result(&json!({ "count": results.len(), "results": results }))
}

// ============================================================================
// experiment_get / experiment_list / experiment_timeline
// ============================================================================

pub async fn tool_experiment_get(
    ctx: &SystemContext,
    params: ExperimentGetParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let core = queries::get_experiment_core(pool, params.experiment_id, params.slug.as_deref())
        .await
        .map_err(|e| McpError::internal_error(format!("get_experiment_core: {e}"), None))?
        .ok_or_else(|| McpError::invalid_params("experiment not found", None))?;

    let hyps = queries::list_experiment_hypotheses(pool, core.id)
        .await
        .map_err(|e| McpError::internal_error(format!("list hypotheses: {e}"), None))?;
    let results = queries::list_experiment_results(pool, core.id)
        .await
        .map_err(|e| McpError::internal_error(format!("list results: {e}"), None))?;

    let hyps_json: Vec<_> = hyps
        .iter()
        .map(|h| {
            json!({
                "hypothesis_id": h.id,
                "statement": h.statement,
                "primary_metric": h.primary_metric,
                "unit": h.unit,
                "predicted_direction": h.predicted_direction,
                "acceptance_criterion": serde_json::from_str::<serde_json::Value>(&h.acceptance_criterion_json).unwrap_or(serde_json::Value::Null),
                "criterion_locked_at": h.criterion_locked_at.to_rfc3339(),
                "planned_n": h.planned_n,
                "verdict": h.verdict,
            })
        })
        .collect();
    let results_json: Vec<_> = results
        .iter()
        .map(|r| {
            json!({
                "result_id": r.id,
                "hypothesis_id": r.hypothesis_id,
                "test_type": r.test_type,
                "metric": r.metric_name,
                "statistic": r.statistic,
                "p_value": r.p_value,
                "effect_size": r.effect_size,
                "ci_low": r.ci_low,
                "ci_high": r.ci_high,
                "verdict": r.verdict,
                "accepted": r.accepted,
                "rationale": r.rationale,
                "decided_at": r.created_at.to_rfc3339(),
            })
        })
        .collect();

    json_result(&json!({
        "experiment_id": core.id,
        "slug": core.slug,
        "title": core.title,
        "question": core.question,
        "context": core.context,
        "kind": core.kind,
        "status": core.status,
        "project": core.project,
        "git_ref": core.git_ref,
        "plan_ref": core.plan_ref,
        "correction": core.correction,
        "created_at": core.created_at.to_rfc3339(),
        "updated_at": core.updated_at.to_rfc3339(),
        "hypotheses": hyps_json,
        "decisions": results_json,
    }))
}

pub async fn tool_experiment_list(
    ctx: &SystemContext,
    params: ExperimentListParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let limit = params.limit.unwrap_or(50).clamp(1, 500) as i64;
    let offset = params.offset.unwrap_or(0).max(0) as i64;
    let rows = queries::list_experiments(
        pool,
        params.project_id,
        params.kind.as_deref(),
        params.status.as_deref(),
        limit,
        offset,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("list_experiments: {e}"), None))?;

    let items: Vec<_> = rows
        .into_iter()
        .map(|r| {
            json!({
                "experiment_id": r.id,
                "slug": r.slug,
                "title": r.title,
                "kind": r.kind,
                "status": r.status,
                "project": r.project,
                "updated_at": r.updated_at.to_rfc3339(),
            })
        })
        .collect();
    json_result(&json!({ "count": items.len(), "experiments": items }))
}

pub async fn tool_experiment_timeline(
    ctx: &SystemContext,
    params: ExperimentTimelineParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let core = queries::get_experiment_core(pool, params.experiment_id, params.slug.as_deref())
        .await
        .map_err(|e| McpError::internal_error(format!("get_experiment_core: {e}"), None))?
        .ok_or_else(|| McpError::invalid_params("experiment not found", None))?;

    let events = queries::experiment_timeline(pool, core.id)
        .await
        .map_err(|e| McpError::internal_error(format!("experiment_timeline: {e}"), None))?;
    let items: Vec<_> = events
        .into_iter()
        .map(|e| {
            json!({
                "at": e.at.to_rfc3339(),
                "event": e.event,
                "detail": e.detail,
            })
        })
        .collect();
    json_result(&json!({
        "experiment_id": core.id,
        "slug": core.slug,
        "timeline": items,
    }))
}

// ============================================================================
// experiment_log_artifact
// ============================================================================

pub async fn tool_experiment_log_artifact(
    ctx: &SystemContext,
    params: ExperimentLogArtifactParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    if params.kind.trim().is_empty() {
        return Err(McpError::invalid_params("kind must be non-empty", None));
    }
    let content = params.content.as_deref();
    let sha = content.map(|c| format!("{:x}", Sha256::digest(c.as_bytes())));

    // Parse recognized benchmark formats into a metrics summary.
    let mut metrics = params.metrics.clone().unwrap_or_else(|| json!({}));
    let mut parsed_sample_count: Option<usize> = None;
    if params.parse.unwrap_or(false)
        && let Some(c) = content
    {
        let parsed = match params.kind.as_str() {
            "hyperfine" => extract::parse_hyperfine_times(c).ok(),
            "criterion" => extract::parse_criterion_samples(c).ok(),
            _ => None,
        };
        if let Some(samples) = parsed {
            let s = inference::summarize(&samples);
            metrics = json!({
                "n": s.n, "mean": s.mean, "median": s.median,
                "std_dev": s.std_dev, "min": s.min, "max": s.max,
            });
            parsed_sample_count = Some(samples.len());
        }
    }
    let metrics_json = metrics.to_string();

    let cfg = ctx.config().load();
    let embed_on_write = cfg.experiments.embed_on_write;
    drop(cfg);
    let label_part = params
        .label
        .as_deref()
        .map(|l| format!(" '{l}'"))
        .unwrap_or_default();
    let snippet: String = content
        .map(|c| c.chars().take(280).collect())
        .unwrap_or_default();
    let summary = format!("{} artifact{label_part}: {snippet}", params.kind);
    let embedding = embed_opt(ctx, embed_on_write, &summary).await;

    let artifact_id = queries::insert_experiment_artifact(
        pool,
        params.experiment_id,
        params.project_id,
        &params.kind,
        params.tool.as_deref(),
        params.label.as_deref(),
        content,
        sha.as_deref(),
        &metrics_json,
        params.file_id,
        embedding,
        params.git_ref.as_deref(),
    )
    .await
    .map_err(|e| McpError::internal_error(format!("insert_experiment_artifact: {e}"), None))?;

    ctx.stats()
        .experiment_artifacts_logged
        .fetch_add(1, Ordering::Relaxed);

    json_result(&json!({
        "artifact_id": artifact_id,
        "kind": params.kind,
        "parsed_metrics": metrics,
        "parsed_sample_count": parsed_sample_count,
    }))
}

// ============================================================================
// experiment_render_ledger
// ============================================================================

pub async fn tool_experiment_render_ledger(
    ctx: &SystemContext,
    params: ExperimentRenderLedgerParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let cfg = ctx.config().load();
    let ledger_dir = cfg.experiments.ledger_dir.clone();
    drop(cfg);
    let base_dir =
        std::env::current_dir().map_err(|e| McpError::internal_error(format!("cwd: {e}"), None))?;
    let dry_run = params.dry_run.unwrap_or(false);

    let rendered = ledger::render_and_write(
        pool,
        params.experiment_id,
        params.slug.as_deref(),
        &ledger_dir,
        &base_dir,
        dry_run,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("render_ledger: {e}"), None))?;

    ctx.stats()
        .experiment_ledgers_rendered
        .fetch_add(1, Ordering::Relaxed);

    json_result(&json!({
        "path": rendered.path.display().to_string(),
        "written": rendered.written,
        "bytes": rendered.content.len(),
        "content": if dry_run { Some(rendered.content) } else { None },
    }))
}
