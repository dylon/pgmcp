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

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use pgvector::Vector;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use sha2::{Digest, Sha256};

use crate::context::SystemContext;
use crate::db::queries::{self, InsertExperimentResult};
use crate::experiment::vocab::{
    EffectDirection, ExperimentArmKind, ExperimentKind, ExperimentStatus, HypothesisVerdict,
};
use crate::experiment::{extract, ledger, mirror, protocol};
use crate::mcp::server::{
    ExperimentDecideParams, ExperimentGetParams, ExperimentListParams, ExperimentLogArtifactParams,
    ExperimentOpenParams, ExperimentProtocolParams, ExperimentRecordMeasurementParams,
    ExperimentRenderLedgerParams, ExperimentSearchParams, ExperimentTimelineParams,
};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::stats::acceptance::{self, AcceptanceCriterion};
use crate::stats::inference::{self, Correction, TestResult};

// kind / status / verdict / arm_kind / predicted_direction vocabularies live in
// `crate::experiment::vocab` (the ADR-003 single source of truth shared with the
// DB CHECK constraints). `VALID_MEASUREMENT_SOURCES` is a separate, non-enum
// vocabulary and stays here.
const VALID_MEASUREMENT_SOURCES: &[&str] = &[
    "external_benchmark",
    "pgmcp_metric",
    "agent_scalar",
    "manual",
];
const MAX_MEASUREMENT_SAMPLES: usize = 10_000;
const MAX_EXPERIMENT_DECIDE_LABEL_BYTES: usize = 128;
const MAX_EXPERIMENT_DECIDE_TEXT_BYTES: usize = 4096;

fn normalize_decide_label(
    value: Option<String>,
    default: &str,
    field: &str,
) -> Result<String, McpError> {
    let value = value.unwrap_or_else(|| default.to_string());
    let value = value.trim();
    if value.is_empty() {
        return Err(McpError::invalid_params(
            format!("{field} must not be blank"),
            None,
        ));
    }
    if value.len() > MAX_EXPERIMENT_DECIDE_LABEL_BYTES {
        return Err(McpError::invalid_params(
            format!("{field} must be at most {MAX_EXPERIMENT_DECIDE_LABEL_BYTES} bytes"),
            None,
        ));
    }
    Ok(value.to_string())
}

fn normalize_optional_decide_text(
    value: Option<String>,
    field: &str,
) -> Result<Option<String>, McpError> {
    match value {
        Some(value) => {
            let value = value.trim();
            if value.is_empty() {
                return Ok(None);
            }
            if value.len() > MAX_EXPERIMENT_DECIDE_TEXT_BYTES {
                return Err(McpError::invalid_params(
                    format!("{field} must be at most {MAX_EXPERIMENT_DECIDE_TEXT_BYTES} bytes"),
                    None,
                ));
            }
            Ok(Some(value.to_string()))
        }
        None => Ok(None),
    }
}

/// Embed `text` when `on`, mapping failures to `None` (the migration cron
/// backfills NULL embeddings, so a transient embed failure is not fatal).
async fn embed_opt(ctx: &SystemContext, on: bool, text: &str) -> Option<Vector> {
    if !on || text.trim().is_empty() {
        return None;
    }
    match ctx.embed().embed_query(text).await {
        Ok(v) => Some(Vector::from(v)),
        Err(e) => {
            tracing::error!(error = %e, "experiment embed-on-write failed; leaving NULL for cron backfill");
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

fn nonblank_str(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

async fn validate_project_id(
    pool: &sqlx::PgPool,
    project_id: Option<i32>,
) -> Result<Option<i32>, McpError> {
    let Some(project_id) = project_id else {
        return Ok(None);
    };
    if project_id <= 0 {
        return Err(McpError::invalid_params(
            "project_id must be positive",
            None,
        ));
    }
    let exists =
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM projects WHERE id = $1)")
            .bind(project_id)
            .fetch_one(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("validate project_id: {e}"), None))?;
    if exists {
        Ok(Some(project_id))
    } else {
        Err(McpError::invalid_params(
            format!("unknown project_id {project_id}"),
            None,
        ))
    }
}

fn map_experiment_open_err(e: sqlx::Error) -> McpError {
    if let Some(db) = e.as_database_error() {
        let code = db.code().map(|code| code.into_owned());
        return match code.as_deref() {
            Some("23503") => McpError::invalid_params("referenced project disappeared", None),
            Some("23514") | Some("22P02") => {
                McpError::invalid_params("experiment rejected by DB constraint", None)
            }
            _ => McpError::internal_error(format!("experiment_open: {e}"), None),
        };
    }
    McpError::internal_error(format!("experiment_open: {e}"), None)
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

    let title = params.title.trim();
    let question = params.question.trim();
    let hypothesis = params.hypothesis.trim();
    let primary_metric = params.primary_metric.trim();
    if title.is_empty() || question.is_empty() || hypothesis.is_empty() || primary_metric.is_empty()
    {
        return Err(McpError::invalid_params(
            "title, question, hypothesis, and primary_metric must be non-empty",
            None,
        ));
    }
    let kind = nonblank_str(params.kind.as_deref()).unwrap_or("other");
    if ExperimentKind::parse(kind).is_none() {
        let allowed: Vec<&str> = ExperimentKind::ALL.iter().map(|k| k.as_str()).collect();
        return Err(McpError::invalid_params(
            format!("unknown kind '{kind}'; expected one of {allowed:?}"),
            None,
        ));
    }
    let predicted_direction =
        nonblank_str(params.predicted_direction.as_deref()).unwrap_or("either");
    if EffectDirection::parse(predicted_direction).is_none() {
        let allowed: Vec<&str> = EffectDirection::ALL.iter().map(|d| d.as_str()).collect();
        return Err(McpError::invalid_params(
            format!(
                "unknown predicted_direction '{predicted_direction}'; expected one of {allowed:?}"
            ),
            None,
        ));
    }
    let project_id = validate_project_id(pool, params.project_id).await?;
    let context = nonblank_str(params.context.as_deref());
    let unit = nonblank_str(params.unit.as_deref());
    let git_ref = nonblank_str(params.git_ref.as_deref());
    let plan_ref = nonblank_str(params.plan_ref.as_deref());

    // Resolve the acceptance criterion: supplied JSON, or a kind-appropriate
    // default (Welch p<0.05 ∧ |d|≥0.5 ∧ correct direction).
    let criterion: AcceptanceCriterion = match &params.acceptance_criterion {
        Some(v) => {
            // Tolerate a JSON-encoded STRING: some MCP argument encoders stringify
            // a nested object passed to an untyped (schema-less) param, so a caller
            // sending `{"type":"wilcoxon_signed_rank",...}` may arrive as a string.
            // Parse the string to a Value first, then deserialize, so both the
            // object and the string-encoded form are accepted.
            let value = match v {
                serde_json::Value::String(s) => serde_json::from_str::<serde_json::Value>(s)
                    .map_err(|e| {
                        McpError::invalid_params(
                            format!("acceptance_criterion was a string but not valid JSON: {e}"),
                            None,
                        )
                    })?,
                other => other.clone(),
            };
            serde_json::from_value(value).map_err(|e| {
                McpError::invalid_params(format!("invalid acceptance_criterion: {e}"), None)
            })?
        }
        None => AcceptanceCriterion::default_optimization(params.lower_is_better.unwrap_or(true)),
    };
    let criterion_json = serde_json::to_string(&criterion)
        .map_err(|e| McpError::internal_error(format!("serialize criterion: {e}"), None))?;

    let cfg = ctx.config().load();
    let exp_cfg = &cfg.experiments;

    // Prescribe the protocol (kind-aware; sizes the sample via power analysis).
    let proto = protocol::prescribe(
        kind,
        primary_metric,
        unit,
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
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| slugify(title));
    let hardware_json = params
        .hardware
        .as_ref()
        .map(|v| v.to_string())
        .unwrap_or_else(|| "{}".to_string());

    // Embeddings (synchronous on write; cron backfills on failure).
    let exp_text = format!("{} {} {}", title, question, context.unwrap_or(""));
    let exp_embedding = embed_opt(ctx, embed_on_write, &exp_text).await;
    let hyp_embedding = embed_opt(ctx, embed_on_write, hypothesis).await;

    let mut tx = pool.begin().await.map_err(map_experiment_open_err)?;
    let experiment_id = queries::insert_experiment_in_tx(
        &mut tx,
        &slug,
        title,
        question,
        context,
        kind,
        project_id,
        &hardware_json,
        git_ref,
        plan_ref,
        &correction,
        exp_embedding,
    )
    .await
    .map_err(map_experiment_open_err)?;

    let hypothesis_id = queries::insert_experiment_hypothesis_in_tx(
        &mut tx,
        experiment_id,
        hypothesis,
        primary_metric,
        unit,
        predicted_direction,
        &criterion_json,
        planned_n,
        hyp_embedding,
    )
    .await
    .map_err(map_experiment_open_err)?;
    tx.commit().await.map_err(map_experiment_open_err)?;

    // Code anchors (best-effort path resolution).
    let mut anchored = 0usize;
    if let Some(paths) = &params.anchor_paths {
        for path in paths {
            let Some(path) = nonblank_str(Some(path.as_str())) else {
                continue;
            };
            if let Ok(Some(file_id)) =
                queries::resolve_experiment_file_id(pool, project_id, path).await
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
        title,
        question,
        kind,
        project_id,
        hypothesis_id,
        hypothesis,
        primary_metric,
    )
    .await
    {
        tracing::error!(error = %e, "experiment mirror_open failed (non-fatal)");
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

    let experiment_id = params.experiment_id;
    if let Some(id) = experiment_id
        && id <= 0
    {
        return Err(McpError::invalid_params(
            "experiment_id must be positive",
            None,
        ));
    }
    let slug = nonblank_str(params.slug.as_deref());
    if experiment_id.is_none() && slug.is_none() {
        return Err(McpError::invalid_params(
            "experiment_id or slug is required",
            None,
        ));
    }

    let core = queries::get_experiment_core(pool, experiment_id, slug)
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

    let arm_label = params.arm_label.trim();
    if arm_label.is_empty() {
        return Err(McpError::invalid_params(
            "arm_label must be non-empty",
            None,
        ));
    }
    let arm_kind = params.arm_kind.trim();
    if ExperimentArmKind::parse(arm_kind).is_none() {
        let allowed: Vec<&str> = ExperimentArmKind::ALL.iter().map(|a| a.as_str()).collect();
        return Err(McpError::invalid_params(
            format!("arm_kind must be one of {allowed:?}"),
            None,
        ));
    }
    let metric = params.metric.trim();
    if metric.is_empty() {
        return Err(McpError::invalid_params("metric must be non-empty", None));
    }
    if params.samples.is_empty() {
        return Err(McpError::invalid_params("samples must be non-empty", None));
    }
    if params.samples.len() > MAX_MEASUREMENT_SAMPLES {
        return Err(McpError::invalid_params(
            format!("samples length must be <= {MAX_MEASUREMENT_SAMPLES}"),
            None,
        ));
    }
    if params.samples.iter().any(|v| !v.is_finite()) {
        return Err(McpError::invalid_params("samples must all be finite", None));
    }
    let source = params
        .source
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("manual");
    if !VALID_MEASUREMENT_SOURCES.contains(&source) {
        return Err(McpError::invalid_params(
            format!("source must be one of {VALID_MEASUREMENT_SOURCES:?}"),
            None,
        ));
    }
    let normalized_unit_keys = match &params.unit_keys {
        Some(keys) => {
            if keys.len() != params.samples.len() {
                return Err(McpError::invalid_params(
                    "unit_keys length must equal samples length",
                    None,
                ));
            }
            let mut seen = HashSet::with_capacity(keys.len());
            let mut normalized = Vec::with_capacity(keys.len());
            for key in keys {
                let key = key.trim();
                if key.is_empty() {
                    return Err(McpError::invalid_params(
                        "unit_keys entries must be non-empty",
                        None,
                    ));
                }
                if !seen.insert(key.to_string()) {
                    return Err(McpError::invalid_params(
                        "unit_keys entries must be unique within a submission",
                        None,
                    ));
                }
                normalized.push(key.to_string());
            }
            Some(normalized)
        }
        None => None,
    };
    let unit = params
        .unit
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let core = queries::get_experiment_core(pool, Some(params.experiment_id), None)
        .await
        .map_err(|e| McpError::internal_error(format!("get_experiment_core: {e}"), None))?
        .ok_or_else(|| McpError::invalid_params("experiment not found", None))?;
    let hypothesis = match params.hypothesis_id {
        Some(hyp_id) => {
            let hyp = queries::get_experiment_hypothesis(pool, hyp_id)
                .await
                .map_err(|e| {
                    McpError::internal_error(format!("get_experiment_hypothesis: {e}"), None)
                })?
                .ok_or_else(|| McpError::invalid_params("hypothesis not found", None))?;
            if hyp.experiment_id != params.experiment_id {
                return Err(McpError::invalid_params(
                    "hypothesis_id does not belong to experiment_id",
                    None,
                ));
            }
            Some(hyp)
        }
        None => None,
    };

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

    let recorded = queries::record_experiment_measurement(
        pool,
        queries::RecordExperimentMeasurement {
            experiment_id: params.experiment_id,
            hypothesis_id: params.hypothesis_id,
            arm_label,
            arm_kind,
            command_spec_json: &command_spec,
            run_plan_json: &run_plan,
            host_meta_json: &host_meta,
            git_ref: params.git_ref.as_deref(),
            runner: Some(source),
            seed: params.seed.unwrap_or(0),
            metric_name: metric,
            samples: &params.samples,
            unit_keys: normalized_unit_keys.as_deref(),
            is_warmup: params.is_warmup.unwrap_or(false),
        },
    )
    .await
    .map_err(|e| McpError::internal_error(format!("record_experiment_measurement: {e}"), None))?;

    let is_warmup = params.is_warmup.unwrap_or(false);
    // Summary over the just-submitted samples. Conformance below excludes
    // warm-up rows from protocol sample counts and statistical decisions.
    let summary = inference::summarize(&params.samples);

    // Conformance check: compare the total non-warm-up samples recorded so far
    // for this arm/metric against the protocol's required_samples_per_arm.
    let mut conformance = json!({ "checked": false });
    if !is_warmup
        && let Some(hyp) = hypothesis.as_ref()
        && let Ok(criterion) =
            serde_json::from_str::<AcceptanceCriterion>(&hyp.acceptance_criterion_json)
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
            arm_label,
            metric,
        )
        .await
        .map(|v| v.len())
        .unwrap_or(0);
        let required = proto.required_samples_per_arm;
        let met = required.map(|r| total as u32 >= r).unwrap_or(true);
        // metric/unit-match conformance: a submitted unit that conflicts with
        // the hypothesis's declared unit is flagged — samples in the wrong unit
        // would silently corrupt the test.
        let unit_match = match (unit, hyp.unit.as_deref()) {
            (Some(submitted), Some(expected)) => submitted == expected,
            _ => true,
        };
        let unit_warning = (!unit_match).then(|| {
            format!(
                "submitted unit {:?} does not match the hypothesis's declared unit {:?}",
                unit, hyp.unit
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
                json!(format!("only {total} non-warm-up samples for arm '{arm_label}'; protocol prescribes >= {required:?}"))
            },
            "unit_warning": unit_warning,
        });
    }

    ctx.stats()
        .experiment_measurements_recorded
        .fetch_add(1, Ordering::Relaxed);

    json_result(&json!({
        "run_id": recorded.run_id,
        "inserted_samples": recorded.inserted_samples,
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
    let ExperimentDecideParams {
        hypothesis_id,
        metric,
        control_arm,
        treatment_arm,
        decided_by,
        rationale_note,
        link_outcome,
    } = params;
    if hypothesis_id <= 0 {
        return Err(McpError::invalid_params(
            "hypothesis_id must be positive",
            None,
        ));
    }

    let hyp = queries::get_experiment_hypothesis(pool, hypothesis_id)
        .await
        .map_err(|e| McpError::internal_error(format!("get_experiment_hypothesis: {e}"), None))?
        .ok_or_else(|| McpError::invalid_params("hypothesis not found", None))?;
    let core = queries::get_experiment_core(pool, Some(hyp.experiment_id), None)
        .await
        .map_err(|e| McpError::internal_error(format!("get_experiment_core: {e}"), None))?
        .ok_or_else(|| McpError::invalid_params("experiment not found", None))?;

    let criterion: AcceptanceCriterion = serde_json::from_str(&hyp.acceptance_criterion_json)
        .map_err(|e| McpError::internal_error(format!("stored criterion parse: {e}"), None))?;
    let metric = normalize_decide_label(metric, &hyp.primary_metric, "metric")?;
    let control_arm = normalize_decide_label(control_arm, "control", "control_arm")?;
    let treatment_arm = normalize_decide_label(treatment_arm, "treatment", "treatment_arm")?;
    if control_arm == treatment_arm {
        return Err(McpError::invalid_params(
            "control_arm and treatment_arm must differ",
            None,
        ));
    }
    let decided_by = normalize_optional_decide_text(decided_by, "decided_by")?;
    let rationale_note = normalize_optional_decide_text(rationale_note, "rationale_note")?;

    // Anti-p-hacking guard: the criterion must predate the first measurement.
    if let Ok(Some(first)) = queries::earliest_measurement_time(pool, hypothesis_id).await
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
        Some(hypothesis_id),
        &control_arm,
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
        Some(hypothesis_id),
        &treatment_arm,
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
        match &rationale_note {
            Some(note) => format!("{base}\n\nOperator note: {note}"),
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

    let control_run_id =
        queries::find_experiment_run_id(pool, hyp.experiment_id, Some(hypothesis_id), &control_arm)
            .await
            .ok()
            .flatten();
    let treatment_run_id = queries::find_experiment_run_id(
        pool,
        hyp.experiment_id,
        Some(hypothesis_id),
        &treatment_arm,
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
        hypothesis_id,
        &metric,
        &verdict,
        &summary,
    )
    .await
    .unwrap_or(None);

    let criterion_snapshot = serde_json::to_string(&criterion).unwrap_or_else(|_| "{}".to_string());
    let result_id = queries::insert_experiment_decision(
        pool,
        InsertExperimentResult {
            experiment_id: hyp.experiment_id,
            hypothesis_id,
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
            decided_by: decided_by.as_deref(),
            embedding: decision_embedding,
            observation_id,
        },
    )
    .await
    .map_err(|e| McpError::internal_error(format!("insert_experiment_result: {e}"), None))?;

    // Phase 10 — tracker bridge: post the verdict to any linked work_items as
    // trusted (source='experiment') verification evidence and auto-verify an
    // accepted hypothesis. Best-effort: a sync failure must not fail the
    // decision (the experiment record is already the source of truth).
    let wi_detail = json!({
        "verdict": verdict,
        "accepted": accepted,
        "test_type": test_type,
        "statistic": statistic,
        "p_value": p_value,
        "effect_size": effect_size,
        "ci_low": ci_low,
        "ci_high": ci_high,
        "criterion_snapshot": serde_json::from_str::<serde_json::Value>(&criterion_snapshot).unwrap_or(serde_json::Value::Null),
    })
    .to_string();
    match queries::sync_experiment_verdict_to_work_items(
        pool,
        hyp.experiment_id,
        &verdict,
        &wi_detail,
    )
    .await
    {
        Ok(n) if n > 0 => {
            tracing::info!(experiment_id = hyp.experiment_id, synced = n, verdict = %verdict,
                "experiment_decide: synced verdict to linked work_items")
        }
        Ok(_) => {}
        Err(e) => tracing::error!(error = %e, "experiment_decide: work_item verdict sync failed"),
    }

    // Optional: graduate a confirmed/rejected verdict into the cross-agent
    // best-practice ledger (consensus → durable-mandate pipeline).
    let link = link_outcome.unwrap_or(true);
    let mut linked_outcome_id: Option<i64> = None;
    if link && matches!(verdict.as_str(), "accepted" | "rejected") {
        use crate::a2a::best_practices::{Outcome, OutcomeReport, record_outcome};
        let outcome = if verdict == "accepted" {
            Outcome::Worked
        } else {
            Outcome::Failed
        };
        let report = OutcomeReport {
            agent_id: decided_by
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
            Err(e) => tracing::error!(error = %e, "experiment_decide: agent_outcomes link failed"),
        }
    }

    ctx.stats()
        .experiment_decisions_made
        .fetch_add(1, Ordering::Relaxed);

    json_result(&json!({
        "result_id": result_id,
        "experiment_id": hyp.experiment_id,
        "hypothesis_id": hypothesis_id,
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

const EXPERIMENT_SEARCH_DEFAULT_LIMIT: i32 = 20;
const EXPERIMENT_SEARCH_MAX_LIMIT: i32 = 100;
const EXPERIMENT_LIST_DEFAULT_LIMIT: i32 = 50;
const EXPERIMENT_LIST_MAX_LIMIT: i32 = 500;

fn normalize_experiment_kind_filter(raw: Option<&str>) -> Result<Option<String>, McpError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let kind = raw.trim().to_ascii_lowercase();
    if kind.is_empty() {
        return Ok(None);
    }
    if ExperimentKind::parse(&kind).is_some() {
        Ok(Some(kind))
    } else {
        let allowed = ExperimentKind::ALL
            .iter()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Err(McpError::invalid_params(
            format!("kind must be one of {allowed}"),
            None,
        ))
    }
}

fn normalize_experiment_verdict_filter(raw: Option<&str>) -> Result<Option<String>, McpError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let verdict = raw.trim().to_ascii_lowercase();
    if verdict.is_empty() {
        return Ok(None);
    }
    if HypothesisVerdict::parse(&verdict).is_some() {
        Ok(Some(verdict))
    } else {
        let allowed = HypothesisVerdict::ALL
            .iter()
            .map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Err(McpError::invalid_params(
            format!("verdict must be one of {allowed}"),
            None,
        ))
    }
}

fn normalize_experiment_status_filter(raw: Option<&str>) -> Result<Option<String>, McpError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let status = raw.trim().to_ascii_lowercase();
    if status.is_empty() {
        return Ok(None);
    }
    if ExperimentStatus::parse(&status).is_some() {
        Ok(Some(status))
    } else {
        let allowed = ExperimentStatus::ALL
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Err(McpError::invalid_params(
            format!("status must be one of {allowed}"),
            None,
        ))
    }
}

fn normalize_experiment_project_filter(project_id: Option<i32>) -> Result<Option<i32>, McpError> {
    match project_id {
        Some(id) if id <= 0 => Err(McpError::invalid_params(
            "project_id must be a positive integer",
            None,
        )),
        other => Ok(other),
    }
}

pub async fn tool_experiment_search(
    ctx: &SystemContext,
    params: ExperimentSearchParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .experiment_searches
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let query = params.query.trim().to_string();
    if query.is_empty() {
        return Err(McpError::invalid_params("query must be non-empty", None));
    }
    let limit = params
        .limit
        .unwrap_or(EXPERIMENT_SEARCH_DEFAULT_LIMIT)
        .clamp(1, EXPERIMENT_SEARCH_MAX_LIMIT) as i64;
    let kind = normalize_experiment_kind_filter(params.kind.as_deref())?;
    let verdict = normalize_experiment_verdict_filter(params.verdict.as_deref())?;

    // Prefer vector search over the experiment embeddings; fall back to FTS.
    let (search_mode, hits) = match ctx.embed().embed_query(&query).await {
        Ok(v) => {
            let qvec = Vector::from(v);
            (
                "vector",
                queries::experiment_search_vector(
                    pool,
                    &qvec,
                    params.project_id,
                    kind.as_deref(),
                    verdict.as_deref(),
                    limit,
                )
                .await,
            )
        }
        Err(_) => (
            "fts",
            queries::experiment_search_fts(
                pool,
                &query,
                params.project_id,
                kind.as_deref(),
                verdict.as_deref(),
                limit,
            )
            .await,
        ),
    };
    let hits =
        hits.map_err(|e| McpError::internal_error(format!("experiment_search: {e}"), None))?;

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
    json_result(&json!({
        "query": query,
        "project_id": params.project_id,
        "kind": kind,
        "verdict": verdict,
        "limit": limit,
        "search_mode": search_mode,
        "count": results.len(),
        "results": results
    }))
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

    let project_id = normalize_experiment_project_filter(params.project_id)?;
    let kind = normalize_experiment_kind_filter(params.kind.as_deref())?;
    let status = normalize_experiment_status_filter(params.status.as_deref())?;
    let limit = params
        .limit
        .unwrap_or(EXPERIMENT_LIST_DEFAULT_LIMIT)
        .clamp(1, EXPERIMENT_LIST_MAX_LIMIT) as i64;
    let offset = params.offset.unwrap_or(0).max(0) as i64;
    let rows = queries::list_experiments(
        pool,
        project_id,
        kind.as_deref(),
        status.as_deref(),
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
    json_result(&json!({
        "count": items.len(),
        "limit": limit,
        "offset": offset,
        "filters": {
            "project_id": project_id,
            "kind": kind,
            "status": status,
        },
        "experiments": items,
    }))
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

    let kind = params.kind.trim();
    if kind.is_empty() {
        return Err(McpError::invalid_params("kind must be non-empty", None));
    }
    let content = params.content.as_deref();
    let sha = content.map(|c| format!("{:x}", Sha256::digest(c.as_bytes())));

    // Parse recognized benchmark / profile formats into a metrics summary.
    let mut metrics = params.metrics.clone().unwrap_or_else(|| json!({}));
    let mut parsed_sample_count: Option<usize> = None;
    if params.parse.unwrap_or(false)
        && let Some(c) = content
    {
        // Numeric-sample benchmark formats → distributional summary.
        let parsed = match kind {
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
        } else {
            // Profile artifacts → structured hot-symbol / heap summary. These
            // are not numeric-sample vectors, so they carry their own shape.
            match kind {
                "perf" => {
                    let mut entries = extract::parse_perf_report(c);
                    let total = entries.len();
                    entries.truncate(25);
                    let hot: Vec<serde_json::Value> = entries
                        .iter()
                        .map(|e| {
                            json!({
                                "symbol": e.symbol,
                                "self_pct": e.self_pct,
                                "children_pct": e.children_pct,
                                "module": e.module,
                            })
                        })
                        .collect();
                    metrics = json!({
                        "profile_kind": "perf",
                        "symbol_count": total,
                        "hot_symbols": hot,
                    });
                    parsed_sample_count = Some(total);
                }
                "flamegraph" => {
                    let folded = extract::parse_folded_stacks(c);
                    let total_samples: u64 = folded.values().copied().sum();
                    let mut leaves: Vec<(String, u64)> = folded.into_iter().collect();
                    leaves.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
                    let distinct = leaves.len();
                    leaves.truncate(25);
                    let hot: Vec<serde_json::Value> = leaves
                        .iter()
                        .map(|(sym, count)| {
                            let pct = if total_samples > 0 {
                                (*count as f64) * 100.0 / (total_samples as f64)
                            } else {
                                0.0
                            };
                            json!({ "symbol": sym, "samples": count, "self_pct": pct })
                        })
                        .collect();
                    metrics = json!({
                        "profile_kind": "flamegraph",
                        "total_samples": total_samples,
                        "distinct_leaves": distinct,
                        "hot_symbols": hot,
                    });
                    parsed_sample_count = Some(distinct);
                }
                "massif" => {
                    let summary = extract::parse_massif(c);
                    let frames: Vec<serde_json::Value> = summary
                        .top_frames
                        .iter()
                        .map(|f| json!({ "function": f.function, "bytes": f.bytes }))
                        .collect();
                    let frame_count = frames.len();
                    metrics = json!({
                        "profile_kind": "massif",
                        "peak_heap_bytes": summary.peak_heap_bytes,
                        "top_frames": frames,
                    });
                    parsed_sample_count = Some(frame_count);
                }
                _ => {}
            }
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
    let summary = format!("{kind} artifact{label_part}: {snippet}");
    let embedding = embed_opt(ctx, embed_on_write, &summary).await;

    let artifact_id = queries::insert_experiment_artifact(
        pool,
        params.experiment_id,
        params.project_id,
        kind,
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
        "kind": kind,
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
