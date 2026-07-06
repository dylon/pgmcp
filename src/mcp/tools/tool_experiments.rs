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
use crate::db::queries::{self, InsertExperimentResult, PairedBinaryCounts};
use crate::experiment::context_tape::{
    self, CellMeasurement, ContextTapeRunner, TapeMetric, TaskFamily,
};
use crate::experiment::vocab::{
    EffectDirection, ExperimentArmKind, ExperimentKind, ExperimentRunStatus, ExperimentStatus,
    HypothesisVerdict,
};
use crate::experiment::{extract, ledger, mirror, protocol};
use crate::mcp::server::{
    ExperimentDecideParams, ExperimentFinalizeRunParams, ExperimentGetParams, ExperimentListParams,
    ExperimentLogArtifactParams, ExperimentOpenParams, ExperimentPreregisterContextTapeParams,
    ExperimentProtocolParams, ExperimentRecordMeasurementFromArtifactParams,
    ExperimentRecordMeasurementParams, ExperimentRecordPairedBinaryCountsParams,
    ExperimentRenderLedgerParams, ExperimentSearchParams, ExperimentSetRunStatusParams,
    ExperimentTimelineParams,
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
/// Hard cap on the artifact file size parsed by
/// `experiment_record_measurement_from_artifact` (the point of the path-passing
/// API is to avoid huge inline payloads; this bounds the server-side read).
const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
/// `α` for the human-readable "significant?" flag on a server-computed verdict.
const PAIRED_BINARY_ALPHA: f64 = 0.05;

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

/// Infer the owning project when `experiment_open` is called without an explicit
/// `project_id`:
///
/// 1. an explicit `project` NAME (resolved via [`queries::resolve_project_id`]),
///    else
/// 2. the caller's cwd — looked up from `mcp_clients` by the request-scoped MCP
///    session id ([`crate::mcp::server::current_mcp_session`]) — resolved to the
///    most-specific enclosing project via [`queries::resolve_project_by_path`].
///
/// Best-effort by design: any miss (no name / unknown name, no session, cwd
/// outside every project root) yields `None` — a workspace-general experiment,
/// exactly as before this inference existed — so it NEVER fails the open. A real
/// DB fault is logged at `error!` (ADR-021: a swallowed error behind a degraded
/// fallback) but still degrades to `None` rather than aborting the open.
async fn infer_open_project_id(pool: &sqlx::PgPool, project_name: Option<&str>) -> Option<i32> {
    // (1) Explicit project NAME wins over cwd inference.
    if let Some(name) = project_name.map(str::trim).filter(|s| !s.is_empty()) {
        match queries::resolve_project_id(pool, Some(name)).await {
            Ok(Some(id)) => return Some(id),
            Ok(None) => {} // unknown name — fall through to cwd inference
            Err(e) => {
                tracing::error!(error = %e, name, "experiment_open: resolve project name failed");
            }
        }
    }

    // (2) Caller cwd via the request-scoped session id → longest project path.
    let session = crate::mcp::server::current_mcp_session()?;
    let cwd = match sqlx::query_scalar::<_, Option<String>>(
        "SELECT cwd FROM mcp_clients WHERE mcp_session_id = $1",
    )
    .bind(&session)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row.flatten(),
        Err(e) => {
            tracing::error!(error = %e, "experiment_open: mcp_clients cwd lookup failed");
            return None;
        }
    }?;

    match queries::resolve_project_by_path(pool, &cwd).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "experiment_open: resolve_project_by_path failed");
            None
        }
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
    // Explicit `project_id` is validated and wins. When omitted, infer the
    // owning project from an explicit `project` NAME, else the caller's cwd —
    // keeping the existing explicit path working and the no-signal path a
    // workspace-general experiment (project_id = None).
    let project_id = match validate_project_id(pool, params.project_id).await? {
        Some(id) => Some(id),
        None => infer_open_project_id(pool, params.project.as_deref()).await,
    };
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

    // Phase 10 tracker bridge — DISABLED 2026-06-20 (full revert of the experiment→
    // work-item self-verification loophole). `experiment_decide` no longer posts
    // tracker verification evidence nor drives any linked work_item →verified:
    // `experiment_open`/`record_measurement`/`decide` are agent-callable with no
    // token and the agent supplies the measurements, so this let an agent
    // self-verify a bug. Tracker verification stays CI-only (`source='ci'`);
    // `sync_experiment_verdict_to_work_items` is itself now an inert no-op (see
    // src/db/queries/work_items.rs). The experiment record + `agent_outcomes`
    // linkage below are unaffected. Original preserved per the no-silent-disable
    // mandate. See docs/reviews/uncommitted-cowboy-changes-2026-06-20.md.
    /*
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
    */

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
// Thread 5b — experiment-API hardening tools (run finalize/status audit,
// paired-corpus 2×2 + McNemar, artifact ingestion). EXPERIMENT subsystem ONLY:
// none of these touch the work-item tracker or post →verified evidence.
// ============================================================================

/// Resolve an `experiment_slug` to its id, erroring with `invalid_params` when no
/// active experiment carries the slug. Trims the caller input (the slug lookup in
/// `get_experiment_core` is exact, so a stray space would otherwise miss).
async fn resolve_experiment_slug(pool: &sqlx::PgPool, slug: &str) -> Result<i64, McpError> {
    let slug = slug.trim();
    if slug.is_empty() {
        return Err(McpError::invalid_params(
            "experiment_slug must be non-empty",
            None,
        ));
    }
    let core = queries::get_experiment_core(pool, None, Some(slug))
        .await
        .map_err(|e| McpError::internal_error(format!("get_experiment_core: {e}"), None))?
        .ok_or_else(|| {
            McpError::invalid_params(format!("no experiment found for slug '{slug}'"), None)
        })?;
    Ok(core.id)
}

/// Normalize the `changed_by` actor recorded on the audit trail. The tool bodies
/// never receive the MCP `RequestContext`, so the actor comes from an explicit
/// param (an agent/operator label) and defaults to `"mcp"`.
fn normalize_changed_by(value: Option<String>) -> String {
    value
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("mcp")
        .to_string()
}

// ============================================================================
// experiment_record_paired_binary_counts
// ============================================================================

/// Store a paired-corpus 2×2 and return the SERVER-COMPUTED McNemar verdict. The
/// agent supplies the counts; the daemon computes the test (the agent never
/// asserts the verdict). Per-hypothesis: the upsert dedupes on
/// `(experiment, hypothesis, metric)`, so `hypothesis_id` is required.
pub async fn tool_experiment_record_paired_binary_counts(
    ctx: &SystemContext,
    params: ExperimentRecordPairedBinaryCountsParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let experiment_id = resolve_experiment_slug(pool, &params.experiment_slug).await?;
    // The 2×2 is per-hypothesis (the upsert key includes hypothesis_id). Require
    // it so two hypotheses' counts cannot collide on a NULL key.
    if params.hypothesis_id <= 0 {
        return Err(McpError::invalid_params(
            "hypothesis_id is required and must be positive (the paired-binary 2×2 dedupes on \
             (experiment, hypothesis, metric))",
            None,
        ));
    }
    let hypothesis_id = params.hypothesis_id;
    let hyp = queries::get_experiment_hypothesis(pool, hypothesis_id)
        .await
        .map_err(|e| McpError::internal_error(format!("get_experiment_hypothesis: {e}"), None))?
        .ok_or_else(|| McpError::invalid_params("hypothesis not found", None))?;
    if hyp.experiment_id != experiment_id {
        return Err(McpError::invalid_params(
            "hypothesis_id does not belong to experiment_slug",
            None,
        ));
    }
    let metric = params.metric.trim();
    if metric.is_empty() {
        return Err(McpError::invalid_params("metric must be non-empty", None));
    }

    // Counts must be non-negative (also enforced by the table CHECK, but reject
    // here with a clear message before the round-trip).
    for (name, value) in [
        ("both_correct", params.both_correct),
        ("control_only", params.control_only),
        ("treatment_only", params.treatment_only),
        ("both_wrong", params.both_wrong),
    ] {
        if value < 0 {
            return Err(McpError::invalid_params(
                format!("{name} must be >= 0"),
                None,
            ));
        }
    }
    let counts = PairedBinaryCounts {
        both_correct: params.both_correct,
        control_only: params.control_only,
        treatment_only: params.treatment_only,
        both_wrong: params.both_wrong,
    };

    // Resolve optional arm labels to run-id pointers (best-effort; the 2×2 stands
    // alone, the pointers are provenance).
    let control_run_id = match nonblank_str(params.control_arm.as_deref()) {
        Some(arm) => queries::find_experiment_run_id(pool, experiment_id, Some(hypothesis_id), arm)
            .await
            .map_err(|e| McpError::internal_error(format!("find control run: {e}"), None))?,
        None => None,
    };
    let treatment_run_id = match nonblank_str(params.treatment_arm.as_deref()) {
        Some(arm) => queries::find_experiment_run_id(pool, experiment_id, Some(hypothesis_id), arm)
            .await
            .map_err(|e| McpError::internal_error(format!("find treatment run: {e}"), None))?,
        None => None,
    };
    let source = nonblank_str(params.source.as_deref());

    // Server-side McNemar verdict over the supplied 2×2 (counts are non-negative,
    // so the cast to u64 is exact). The daemon is the verdict authority.
    let verdict = inference::mcnemar_test(
        counts.both_correct as u64,
        counts.control_only as u64,
        counts.treatment_only as u64,
        counts.both_wrong as u64,
    )
    .map_err(|e| McpError::invalid_params(format!("mcnemar_test: {e}"), None))?;
    let significant = finite_or_none(verdict.p_value)
        .map(|p| p < PAIRED_BINARY_ALPHA)
        .unwrap_or(false);

    let detail_json = json!({
        "mcnemar": {
            "statistic": finite_or_none(verdict.statistic),
            "p_value": finite_or_none(verdict.p_value),
            "n_discordant": verdict.n_discordant,
            "effect_treatment_minus_control": verdict.effect_treatment_minus_control,
            "exact": verdict.exact,
        },
        "significant_at_alpha": significant,
        "alpha": PAIRED_BINARY_ALPHA,
    })
    .to_string();

    let row_id = queries::upsert_paired_binary_counts(
        pool,
        experiment_id,
        Some(hypothesis_id),
        metric,
        control_run_id,
        treatment_run_id,
        counts,
        source,
        &detail_json,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("upsert_paired_binary_counts: {e}"), None))?;

    json_result(&json!({
        "paired_binary_id": row_id,
        "experiment_id": experiment_id,
        "hypothesis_id": hypothesis_id,
        "metric": metric,
        "counts": {
            "both_correct": counts.both_correct,
            "control_only": counts.control_only,
            "treatment_only": counts.treatment_only,
            "both_wrong": counts.both_wrong,
        },
        "mcnemar": {
            "test": if verdict.exact { "exact_binomial" } else { "mcnemar_chi2_continuity" },
            "statistic": finite_or_none(verdict.statistic),
            "p_value": finite_or_none(verdict.p_value),
            "n_discordant": verdict.n_discordant,
            "effect_treatment_minus_control": verdict.effect_treatment_minus_control,
            "exact": verdict.exact,
        },
        "significant_at_alpha_0_05": significant,
        "note": "Verdict computed server-side from the supplied 2×2; the agent does not assert it.",
    }))
}

// ============================================================================
// experiment_finalize_run
// ============================================================================

/// Seal a measurement run for use in a decision: compute + store its
/// tamper-evident samples digest, set `status='finalized'`, and append to the
/// immutable audit trail. Idempotent (re-running recomputes + re-stamps).
pub async fn tool_experiment_finalize_run(
    ctx: &SystemContext,
    params: ExperimentFinalizeRunParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let experiment_id = resolve_experiment_slug(pool, &params.experiment_slug).await?;
    let arm_label = params.arm_label.trim();
    if arm_label.is_empty() {
        return Err(McpError::invalid_params(
            "arm_label must be non-empty",
            None,
        ));
    }
    let run_id =
        queries::find_experiment_run_id(pool, experiment_id, params.hypothesis_id, arm_label)
            .await
            .map_err(|e| McpError::internal_error(format!("find_experiment_run_id: {e}"), None))?
            .ok_or_else(|| {
                McpError::invalid_params(
                    format!("no run found for arm '{arm_label}' on this experiment/hypothesis"),
                    None,
                )
            })?;
    let reason = params
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("finalized via MCP")
        .to_string();
    let changed_by = normalize_changed_by(params.changed_by);

    let result = queries::finalize_experiment_run(pool, run_id, &changed_by, &reason)
        .await
        .map_err(|e| McpError::internal_error(format!("finalize_experiment_run: {e}"), None))?;

    json_result(&json!({
        "run_id": run_id,
        "experiment_id": experiment_id,
        "arm_label": arm_label,
        "status": "finalized",
        "samples_digest": result.samples_digest,
        "sample_count": result.sample_count,
        "changed_by": changed_by,
        "reason": reason,
    }))
}

// ============================================================================
// experiment_set_run_status
// ============================================================================

/// Audited EXCLUSION of a run from decisions (`invalid` / `superseded` only). The
/// anti-cherry-pick guardrail: any decision that consumed the run is re-opened
/// (its hypothesis verdict reverts to `pending`), so excluding data after a
/// decision can never silently keep the favourable verdict.
pub async fn tool_experiment_set_run_status(
    ctx: &SystemContext,
    params: ExperimentSetRunStatusParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let experiment_id = resolve_experiment_slug(pool, &params.experiment_slug).await?;
    let arm_label = params.arm_label.trim();
    if arm_label.is_empty() {
        return Err(McpError::invalid_params(
            "arm_label must be non-empty",
            None,
        ));
    }
    // Reason is REQUIRED and non-empty (the audit trail must always carry a why).
    let reason = params.reason.trim();
    if reason.is_empty() {
        return Err(McpError::invalid_params(
            "reason is required and must be non-empty",
            None,
        ));
    }
    // Only the two EXCLUSION statuses are settable here. `finalized` has its own
    // tool (it computes the digest); `pending`/`complete` are lifecycle states the
    // measurement path manages, not operator-settable exclusions.
    let new_status = match ExperimentRunStatus::parse(params.status.trim()) {
        Some(s @ (ExperimentRunStatus::Invalid | ExperimentRunStatus::Superseded)) => s,
        Some(ExperimentRunStatus::Finalized) => {
            return Err(McpError::invalid_params(
                "use experiment_finalize_run to finalize a run (it computes the samples digest)",
                None,
            ));
        }
        Some(ExperimentRunStatus::Pending | ExperimentRunStatus::Complete) => {
            return Err(McpError::invalid_params(
                "status must be 'invalid' or 'superseded'; 'pending'/'complete' are managed by the \
                 measurement path, not settable here",
                None,
            ));
        }
        None => {
            return Err(McpError::invalid_params(
                "status must be one of: invalid, superseded",
                None,
            ));
        }
    };
    let changed_by = normalize_changed_by(params.changed_by);

    let run_id =
        queries::find_experiment_run_id(pool, experiment_id, params.hypothesis_id, arm_label)
            .await
            .map_err(|e| McpError::internal_error(format!("find_experiment_run_id: {e}"), None))?
            .ok_or_else(|| {
                McpError::invalid_params(
                    format!("no run found for arm '{arm_label}' on this experiment/hypothesis"),
                    None,
                )
            })?;

    let change = queries::set_experiment_run_status(pool, run_id, new_status, reason, &changed_by)
        .await
        .map_err(|e| McpError::internal_error(format!("set_experiment_run_status: {e}"), None))?;

    let reopened_note = (!change.reopened_decisions.is_empty()).then(|| {
        format!(
            "{} decision(s) consumed this run and were RE-OPENED (their hypothesis verdict reverted \
             to pending); a re-decision is required.",
            change.reopened_decisions.len()
        )
    });

    json_result(&json!({
        "run_id": run_id,
        "experiment_id": experiment_id,
        "arm_label": arm_label,
        "old_status": change.old_status,
        "new_status": change.new_status,
        "reason": reason,
        "changed_by": changed_by,
        "reopened_decisions": change.reopened_decisions,
        "reopened_note": reopened_note,
    }))
}

// ============================================================================
// experiment_record_measurement_from_artifact
// ============================================================================

/// One parsed row: an ordered map of `column → raw string cell`. Insertion order
/// is the header/field order so `unit_key_columns` join deterministically.
type ArtifactRow = Vec<(String, String)>;

/// Split one CSV line into fields, honoring `"`-quoted fields (with `""`
/// escaping) and embedded commas. Total (never panics) on malformed input — an
/// unterminated quote simply consumes to end-of-line.
fn split_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes => {
                // A doubled quote inside a quoted field is a literal quote.
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            }
            '"' if !in_quotes => in_quotes = true,
            ',' if !in_quotes => {
                fields.push(std::mem::take(&mut cur));
            }
            other => cur.push(other),
        }
    }
    fields.push(cur);
    fields
}

/// Parse CSV text (first non-empty line = header) into rows of `(column, cell)`.
/// A short row (fewer cells than headers) pads with empty cells; extra cells are
/// dropped. Blank lines are skipped.
fn parse_csv_rows(content: &str) -> Result<Vec<ArtifactRow>, McpError> {
    let mut lines = content.lines().filter(|l| !l.trim().is_empty());
    let header_line = lines
        .next()
        .ok_or_else(|| McpError::invalid_params("CSV artifact is empty (no header row)", None))?;
    let headers: Vec<String> = split_csv_line(header_line)
        .into_iter()
        .map(|h| h.trim().to_string())
        .collect();
    if headers.iter().all(|h| h.is_empty()) {
        return Err(McpError::invalid_params(
            "CSV header row has no column names",
            None,
        ));
    }
    let mut rows = Vec::new();
    for line in lines {
        let cells = split_csv_line(line);
        let mut row: ArtifactRow = Vec::with_capacity(headers.len());
        for (i, header) in headers.iter().enumerate() {
            let cell = cells.get(i).cloned().unwrap_or_default();
            row.push((header.clone(), cell));
        }
        rows.push(row);
    }
    Ok(rows)
}

/// Parse JSONL text (one JSON object per non-blank line) into rows of
/// `(field, stringified-value)`. Non-object lines reject. Scalar fields stringify
/// to their plain form (a string keeps its value; numbers/bools to their text).
fn parse_jsonl_rows(content: &str) -> Result<Vec<ArtifactRow>, McpError> {
    let mut rows = Vec::new();
    for (lineno, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line).map_err(|e| {
            McpError::invalid_params(
                format!("JSONL line {} is not valid JSON: {e}", lineno + 1),
                None,
            )
        })?;
        let obj = value.as_object().ok_or_else(|| {
            McpError::invalid_params(
                format!("JSONL line {} is not a JSON object", lineno + 1),
                None,
            )
        })?;
        let mut row: ArtifactRow = Vec::with_capacity(obj.len());
        for (k, v) in obj {
            let cell = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => String::new(),
                other => other.to_string(),
            };
            row.push((k.clone(), cell));
        }
        rows.push(row);
    }
    Ok(rows)
}

/// Look up a column's raw cell in a parsed row.
fn row_cell<'a>(row: &'a ArtifactRow, column: &str) -> Option<&'a str> {
    row.iter()
        .find(|(k, _)| k == column)
        .map(|(_, v)| v.as_str())
}

/// Resolve `rel` against the daemon's working directory, canonicalize it, and
/// confirm the canonical path stays WITHIN the canonical working directory.
/// Rejects absolute paths and any `..` traversal that escapes the root — the
/// path-traversal guardrail (no reading `/etc/*`). The same base
/// (`std::env::current_dir`) `experiment_render_ledger` writes under.
fn resolve_artifact_path(rel: &str) -> Result<std::path::PathBuf, McpError> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Err(McpError::invalid_params(
            "artifact_path must be non-empty",
            None,
        ));
    }
    let candidate = std::path::Path::new(rel);
    if candidate.is_absolute() {
        return Err(McpError::invalid_params(
            "artifact_path must be relative to the working directory (absolute paths are rejected)",
            None,
        ));
    }
    let base =
        std::env::current_dir().map_err(|e| McpError::internal_error(format!("cwd: {e}"), None))?;
    let base = base.canonicalize().unwrap_or(base);
    let joined = base.join(candidate);
    // Canonicalize resolves `..` / symlinks; a non-existent file is a clean
    // invalid_params, not an internal error.
    let canonical = joined.canonicalize().map_err(|e| {
        McpError::invalid_params(format!("artifact_path could not be resolved: {e}"), None)
    })?;
    if !canonical.starts_with(&base) {
        return Err(McpError::invalid_params(
            "artifact_path escapes the working directory (path traversal rejected)",
            None,
        ));
    }
    Ok(canonical)
}

/// Parse a benchmark artifact file SERVER-SIDE and ingest its numeric column as
/// samples. The agent passes a (working-directory-relative, canonicalized,
/// containment-checked) path, not a giant inline payload. Rows may be split into
/// per-arm runs by `arm_column`. Non-numeric / empty value cells are skipped and
/// reported.
pub async fn tool_experiment_record_measurement_from_artifact(
    ctx: &SystemContext,
    params: ExperimentRecordMeasurementFromArtifactParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let experiment_id = resolve_experiment_slug(pool, &params.experiment_slug).await?;
    let arm_kind = params
        .arm_kind
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("treatment");
    if ExperimentArmKind::parse(arm_kind).is_none() {
        let allowed: Vec<&str> = ExperimentArmKind::ALL.iter().map(|a| a.as_str()).collect();
        return Err(McpError::invalid_params(
            format!("arm_kind must be one of {allowed:?}"),
            None,
        ));
    }
    let default_arm_label = params.arm_label.trim();
    if default_arm_label.is_empty() {
        return Err(McpError::invalid_params(
            "arm_label must be non-empty",
            None,
        ));
    }
    let metric = params.metric.trim();
    if metric.is_empty() {
        return Err(McpError::invalid_params("metric must be non-empty", None));
    }
    let value_column = params.value_column.trim();
    if value_column.is_empty() {
        return Err(McpError::invalid_params(
            "value_column must be non-empty",
            None,
        ));
    }
    let format = params.format.trim().to_ascii_lowercase();
    if format != "csv" && format != "jsonl" {
        return Err(McpError::invalid_params(
            "format must be 'csv' or 'jsonl'",
            None,
        ));
    }
    let source = nonblank_str(params.source.as_deref()).unwrap_or("external_benchmark");
    if !VALID_MEASUREMENT_SOURCES.contains(&source) {
        return Err(McpError::invalid_params(
            format!("source must be one of {VALID_MEASUREMENT_SOURCES:?}"),
            None,
        ));
    }
    let hypothesis_id = params.hypothesis_id;
    if let Some(hyp_id) = hypothesis_id {
        let hyp = queries::get_experiment_hypothesis(pool, hyp_id)
            .await
            .map_err(|e| McpError::internal_error(format!("get_experiment_hypothesis: {e}"), None))?
            .ok_or_else(|| McpError::invalid_params("hypothesis not found", None))?;
        if hyp.experiment_id != experiment_id {
            return Err(McpError::invalid_params(
                "hypothesis_id does not belong to experiment_slug",
                None,
            ));
        }
    }

    // ── path safety + bounded read (server-side) ──
    let path = resolve_artifact_path(&params.artifact_path)?;
    let meta = tokio::fs::metadata(&path)
        .await
        .map_err(|e| McpError::invalid_params(format!("artifact stat failed: {e}"), None))?;
    if !meta.is_file() {
        return Err(McpError::invalid_params(
            "artifact_path is not a regular file",
            None,
        ));
    }
    if meta.len() > MAX_ARTIFACT_BYTES {
        return Err(McpError::invalid_params(
            format!(
                "artifact is {} bytes; the limit is {MAX_ARTIFACT_BYTES} (pass a pre-summarized file)",
                meta.len()
            ),
            None,
        ));
    }
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| McpError::invalid_params(format!("artifact read failed: {e}"), None))?;

    let rows = match format.as_str() {
        "csv" => parse_csv_rows(&content)?,
        _ => parse_jsonl_rows(&content)?,
    };

    let arm_column = nonblank_str(params.arm_column.as_deref());
    let is_warmup_column = nonblank_str(params.is_warmup_column.as_deref());
    let unit_key_columns: Vec<String> = params
        .unit_key_columns
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    let filters: Vec<(String, String)> = params
        .filters
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| (k.trim().to_string(), v))
        .filter(|(k, _)| !k.is_empty())
        .collect();

    // ── extract samples per (arm) bucket, in stable insertion order ──
    // Each bucket: (samples, unit_keys-if-any). We preserve first-seen arm order.
    let mut buckets: Vec<(String, Vec<f64>, Vec<String>)> = Vec::new();
    let mut skipped_non_numeric = 0usize;
    let mut skipped_filtered = 0usize;
    let mut skipped_warmup_unparsed = 0usize;
    let mut any_unit_key = false;

    'rows: for row in &rows {
        // Row filters (string equality on the raw cell). A missing column fails
        // the filter (the row is excluded), never panics.
        for (k, want) in &filters {
            if row_cell(row, k) != Some(want.as_str()) {
                skipped_filtered += 1;
                continue 'rows;
            }
        }
        // Warm-up flag: a truthy cell marks the row as warm-up. We still ingest it
        // (so it is recorded), but flagged via is_warmup on its own run write
        // below — to keep the per-arm run write simple we route warmups into a
        // separate bucket suffix.
        let is_warmup = match is_warmup_column {
            Some(col) => match row_cell(row, col) {
                Some(cell) => parse_truthy(cell),
                None => {
                    skipped_warmup_unparsed += 1;
                    false
                }
            },
            None => false,
        };

        // Value cell → f64. Empty / non-numeric is skipped + counted.
        let Some(raw_value) = row_cell(row, value_column) else {
            skipped_non_numeric += 1;
            continue;
        };
        let trimmed = raw_value.trim();
        let Ok(value) = trimmed.parse::<f64>() else {
            skipped_non_numeric += 1;
            continue;
        };
        if !value.is_finite() {
            skipped_non_numeric += 1;
            continue;
        }

        // Arm routing: explicit arm_column value (else the default arm_label).
        let arm_value = match arm_column {
            Some(col) => row_cell(row, col)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(default_arm_label),
            None => default_arm_label,
        };
        // Warm-up rows go to a distinct bucket so they are written is_warmup=true
        // without commingling with the steady-state samples of the same arm.
        let bucket_key = if is_warmup {
            format!("{arm_value}\u{1f}warmup")
        } else {
            arm_value.to_string()
        };

        // unit_key = the configured columns joined by '\u{1f}'. Empty when no
        // columns configured (paired alignment off).
        let unit_key = if unit_key_columns.is_empty() {
            String::new()
        } else {
            any_unit_key = true;
            unit_key_columns
                .iter()
                .map(|c| row_cell(row, c).unwrap_or("").to_string())
                .collect::<Vec<_>>()
                .join("\u{1f}")
        };

        match buckets.iter_mut().find(|(k, _, _)| *k == bucket_key) {
            Some((_, samples, keys)) => {
                samples.push(value);
                keys.push(unit_key);
            }
            None => {
                buckets.push((bucket_key, vec![value], vec![unit_key]));
            }
        }
    }

    if buckets.is_empty() {
        return Err(McpError::invalid_params(
            format!(
                "no numeric samples extracted from column '{value_column}' (rows={}, skipped \
                 non-numeric={skipped_non_numeric}, filtered out={skipped_filtered})",
                rows.len()
            ),
            None,
        ));
    }

    // ── write each bucket through the existing measurement path ──
    let host_meta = json!({
        "artifact_path": params.artifact_path,
        "format": format,
        "value_column": value_column,
    })
    .to_string();
    let mut runs_out = Vec::with_capacity(buckets.len());
    let mut total_inserted = 0u64;
    for (bucket_key, samples, keys) in &buckets {
        if samples.len() > MAX_MEASUREMENT_SAMPLES {
            return Err(McpError::invalid_params(
                format!(
                    "arm '{bucket_key}' yielded {} samples; the per-run limit is \
                     {MAX_MEASUREMENT_SAMPLES}",
                    samples.len()
                ),
                None,
            ));
        }
        let is_warmup = bucket_key.ends_with("\u{1f}warmup");
        let arm_label = bucket_key
            .strip_suffix("\u{1f}warmup")
            .unwrap_or(bucket_key);
        let unit_keys: Option<&[String]> = if any_unit_key {
            Some(keys.as_slice())
        } else {
            None
        };
        let recorded = queries::record_experiment_measurement(
            pool,
            queries::RecordExperimentMeasurement {
                experiment_id,
                hypothesis_id,
                arm_label,
                arm_kind,
                command_spec_json: "{}",
                run_plan_json: "{}",
                host_meta_json: &host_meta,
                git_ref: None,
                runner: Some(source),
                seed: 0,
                metric_name: metric,
                samples,
                unit_keys,
                is_warmup,
            },
        )
        .await
        .map_err(|e| {
            McpError::internal_error(format!("record_experiment_measurement: {e}"), None)
        })?;
        total_inserted += recorded.inserted_samples;
        runs_out.push(json!({
            "arm": arm_label,
            "arm_kind": arm_kind,
            "is_warmup": is_warmup,
            "run_id": recorded.run_id,
            "inserted_samples": recorded.inserted_samples,
        }));
    }

    ctx.stats()
        .experiment_measurements_recorded
        .fetch_add(1, Ordering::Relaxed);

    json_result(&json!({
        "experiment_id": experiment_id,
        "metric": metric,
        "format": format,
        "rows_parsed": rows.len(),
        "runs": runs_out,
        "total_inserted_samples": total_inserted,
        "skipped": {
            "non_numeric_or_empty_value": skipped_non_numeric,
            "filtered_out": skipped_filtered,
            "warmup_column_missing": skipped_warmup_unparsed,
        },
        "conformance": {
            "checked": false,
            "note": "per-arm conformance is reported by experiment_record_measurement / experiment_get; \
                     this tool ingests raw samples from the artifact.",
        },
    }))
}

/// Interpret a raw cell as a boolean flag for `is_warmup_column`. Truthy:
/// `true`/`1`/`yes`/`y`/`warmup` (case-insensitive). Everything else is false.
fn parse_truthy(cell: &str) -> bool {
    matches!(
        cell.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "y" | "warmup" | "t"
    )
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

    // Audit-visibility (Thread 5b): every run's status + sample count + digest, so
    // a finalized / invalid / superseded run is visible without a side query. A run
    // whose status is NOT usable in a decision is flagged.
    let runs = queries::experiment_runs_overview(pool, core.id)
        .await
        .map_err(|e| McpError::internal_error(format!("experiment_runs_overview: {e}"), None))?;
    let measurement_runs: Vec<_> = runs
        .iter()
        .map(|r| {
            let usable = ExperimentRunStatus::parse(&r.status)
                .map(|s| s.usable_in_decision())
                .unwrap_or(false);
            json!({
                "run_id": r.run_id,
                "arm_label": r.arm_label,
                "arm_kind": r.arm_kind,
                "status": r.status,
                "usable_in_decision": usable,
                "sample_count": r.sample_count,
                "samples_digest": r.samples_digest,
                "status_reason": r.status_reason,
                "finalized_at": r.finalized_at.map(|t| t.to_rfc3339()),
            })
        })
        .collect();

    // Paired-corpus 2×2 sections: for each hypothesis, surface the stored 2×2 for
    // its primary metric (the per-hypothesis dedupe key), with the server-recomputed
    // McNemar verdict so the audit view never trusts a client-asserted result.
    let mut paired_binary = Vec::with_capacity(hyps.len());
    for h in &hyps {
        if let Some(counts) =
            queries::load_paired_binary_counts(pool, core.id, Some(h.id), &h.primary_metric)
                .await
                .map_err(|e| {
                    McpError::internal_error(format!("load_paired_binary_counts: {e}"), None)
                })?
        {
            let mcnemar = inference::mcnemar_test(
                counts.both_correct.max(0) as u64,
                counts.control_only.max(0) as u64,
                counts.treatment_only.max(0) as u64,
                counts.both_wrong.max(0) as u64,
            )
            .ok();
            paired_binary.push(json!({
                "hypothesis_id": h.id,
                "metric": h.primary_metric,
                "counts": {
                    "both_correct": counts.both_correct,
                    "control_only": counts.control_only,
                    "treatment_only": counts.treatment_only,
                    "both_wrong": counts.both_wrong,
                },
                "mcnemar": mcnemar.as_ref().map(|m| json!({
                    "statistic": finite_or_none(m.statistic),
                    "p_value": finite_or_none(m.p_value),
                    "n_discordant": m.n_discordant,
                    "effect_treatment_minus_control": m.effect_treatment_minus_control,
                    "exact": m.exact,
                    "significant_at_alpha_0_05": finite_or_none(m.p_value)
                        .map(|p| p < PAIRED_BINARY_ALPHA).unwrap_or(false),
                })),
            }));
        }
    }

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
        "measurement_runs": measurement_runs,
        "paired_binary": paired_binary,
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

// ============================================================================
// experiment_preregister_context_tape  (Crucible P9)
// ============================================================================

/// Parse one `cells[]` JSON element into a [`CellMeasurement`], validating the
/// closed arm/family/metric vocabularies (ADR-003) up front.
fn parse_cell(v: &serde_json::Value) -> Result<CellMeasurement, McpError> {
    let obj = v
        .as_object()
        .ok_or_else(|| McpError::invalid_params("each cell must be a JSON object", None))?;
    let arm_str = obj
        .get("arm")
        .and_then(|x| x.as_str())
        .ok_or_else(|| McpError::invalid_params("cell.arm is required", None))?;
    let arm = ExperimentArmKind::parse(arm_str).ok_or_else(|| {
        let allowed: Vec<&str> = ExperimentArmKind::ALL.iter().map(|a| a.as_str()).collect();
        McpError::invalid_params(
            format!("cell.arm '{arm_str}' must be one of {allowed:?}"),
            None,
        )
    })?;
    let family_str = obj
        .get("family")
        .and_then(|x| x.as_str())
        .ok_or_else(|| McpError::invalid_params("cell.family is required", None))?;
    let family = TaskFamily::parse(family_str).ok_or_else(|| {
        let allowed: Vec<&str> = TaskFamily::ALL.iter().map(|f| f.as_str()).collect();
        McpError::invalid_params(
            format!("cell.family '{family_str}' must be one of {allowed:?}"),
            None,
        )
    })?;
    let metric_str = obj
        .get("metric")
        .and_then(|x| x.as_str())
        .ok_or_else(|| McpError::invalid_params("cell.metric is required", None))?;
    let metric = TapeMetric::parse(metric_str).ok_or_else(|| {
        let allowed: Vec<&str> = TapeMetric::ALL.iter().map(|m| m.as_str()).collect();
        McpError::invalid_params(
            format!("cell.metric '{metric_str}' must be one of {allowed:?}"),
            None,
        )
    })?;
    let samples: Vec<f64> = obj
        .get("samples")
        .and_then(|x| x.as_array())
        .ok_or_else(|| McpError::invalid_params("cell.samples must be an array", None))?
        .iter()
        .map(|n| {
            n.as_f64().ok_or_else(|| {
                McpError::invalid_params("cell.samples entries must be numbers", None)
            })
        })
        .collect::<Result<_, _>>()?;
    let cell = CellMeasurement {
        arm,
        family,
        metric,
        samples,
    };
    cell.validate()
        .map_err(|e| McpError::invalid_params(e, None))?;
    Ok(cell)
}

/// JSON description of the frozen pre-registration (echoed on every call so the
/// definition is self-describing and never confused with an executed run).
fn frozen_preregistration_json() -> serde_json::Value {
    let arms: Vec<_> = context_tape::ARMS
        .iter()
        .map(|a| json!({ "arm": a.as_str(), "description": context_tape::arm_description(*a) }))
        .collect();
    let families: Vec<_> = TaskFamily::ALL.iter().map(|f| f.as_str()).collect();
    let metrics: Vec<_> = TapeMetric::ALL
        .iter()
        .map(|m| {
            json!({
                "metric": m.as_str(),
                "unit": m.unit(),
                "lower_is_better": m.lower_is_better(),
            })
        })
        .collect();
    let clauses: Vec<_> = context_tape::clauses()
        .into_iter()
        .map(|c| {
            json!({
                "metric": c.metric.as_str(),
                "control_arm": c.control_arm.as_str(),
                "treatment_arm": c.treatment_arm.as_str(),
                "rationale": c.rationale,
                "criterion": serde_json::to_value(&c.criterion).unwrap_or(serde_json::Value::Null),
            })
        })
        .collect();
    json!({
        "slug": context_tape::EXPERIMENT_SLUG,
        "design": "3x3x5 (arms × task families × metrics)",
        "arms": arms,
        "task_families": families,
        "metrics": metrics,
        "primary_metric": context_tape::PRIMARY_METRIC.as_str(),
        "frozen_criterion": serde_json::to_value(context_tape::frozen_criterion())
            .unwrap_or(serde_json::Value::Null),
        "clauses": clauses,
        // The ADR-003 closed-vocabulary SQL forms (single source of truth shared
        // with any scoped CHECK constraint).
        "vocab_sql": {
            "task_family": TaskFamily::sql_in_list(),
            "metric": TapeMetric::sql_in_list(),
            "arm_kind": ExperimentArmKind::sql_in_list(),
        },
        "dataset_gated_note": context_tape::DATASET_GATED_NOTE,
    })
}

/// `experiment_preregister_context_tape` — open / record / decide / (optionally)
/// promote the **frozen** Context-Tape pre-registration (P9). Reuses the
/// existing experiment-open + measurement + acceptance-evaluation paths; adds
/// only the per-clause routing the single-pair `experiment_decide` cannot do and
/// the default-OFF, verified-gated memory promotion.
pub async fn tool_experiment_preregister_context_tape(
    ctx: &SystemContext,
    params: ExperimentPreregisterContextTapeParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let preregistration = frozen_preregistration_json();
    let project_id = validate_project_id(pool, params.project_id).await?;

    // Resolve / open the experiment + hypothesis the measurements attach to.
    let mut experiment_id = params.experiment_id;
    let mut hypothesis_id = params.hypothesis_id;
    let mut opened = serde_json::Value::Null;

    if params.open.unwrap_or(false) {
        let criterion = context_tape::frozen_criterion();
        let criterion_json = serde_json::to_string(&criterion)
            .map_err(|e| McpError::internal_error(format!("serialize criterion: {e}"), None))?;
        let cfg = ctx.config().load();
        let exp_cfg = &cfg.experiments;
        let proto = protocol::prescribe(
            "feature_addition",
            context_tape::PRIMARY_METRIC.as_str(),
            context_tape::PRIMARY_METRIC.unit(),
            "increase",
            &criterion,
            exp_cfg,
            None,
        );
        let planned_n = proto.required_samples_per_arm.map(|n| n as i32);
        let correction = exp_cfg.default_correction.clone();
        let embed_on_write = exp_cfg.embed_on_write;
        drop(cfg);

        let title = "Crucible Context-Tape 3x3x5 pre-registration";
        let question = "Does the context tape + paging improve accuracy at equivalent cost, bounded p95 \
             latency, and ≥2× the baseline's max context?";
        let context = Some(context_tape::DATASET_GATED_NOTE);
        let hypothesis = "Treatment (tape+paging) accuracy > control AND cost-equivalent within \
                          20% AND p95 latency ≤ SLO AND max-context ≥ 2× baseline.";

        let exp_text = format!("{title} {question}");
        let exp_embedding = embed_opt(ctx, embed_on_write, &exp_text).await;
        let hyp_embedding = embed_opt(ctx, embed_on_write, hypothesis).await;

        let mut tx = pool.begin().await.map_err(map_experiment_open_err)?;
        let new_exp = queries::insert_experiment_in_tx(
            &mut tx,
            context_tape::EXPERIMENT_SLUG,
            title,
            question,
            context,
            "feature_addition",
            project_id,
            "{}",
            None,
            None,
            &correction,
            exp_embedding,
        )
        .await
        .map_err(map_experiment_open_err)?;
        let new_hyp = queries::insert_experiment_hypothesis_in_tx(
            &mut tx,
            new_exp,
            hypothesis,
            context_tape::PRIMARY_METRIC.as_str(),
            context_tape::PRIMARY_METRIC.unit(),
            "increase",
            &criterion_json,
            planned_n,
            hyp_embedding,
        )
        .await
        .map_err(map_experiment_open_err)?;
        tx.commit().await.map_err(map_experiment_open_err)?;

        // Mirror into the memory graph (best-effort) so the promotion path has a
        // real observation to supersede and the experiment participates in
        // retrieval. A mirror failure must not fail the open (ADR-021 error!).
        if let Err(e) = mirror::mirror_open(
            pool,
            context_tape::EXPERIMENT_SLUG,
            title,
            question,
            "feature_addition",
            project_id,
            new_hyp,
            hypothesis,
            context_tape::PRIMARY_METRIC.as_str(),
        )
        .await
        {
            tracing::error!(error = %e, "context-tape preregister mirror_open failed (non-fatal)");
        }

        ctx.stats()
            .experiments_opened
            .fetch_add(1, Ordering::Relaxed);
        experiment_id = Some(new_exp);
        hypothesis_id = Some(new_hyp);
        opened = json!({
            "experiment_id": new_exp,
            "hypothesis_id": new_hyp,
            "slug": context_tape::EXPERIMENT_SLUG,
            "criterion_locked": true,
            "protocol": proto,
        });
    }

    // The config correction is shared by the record and decide phases.
    let cfg = ctx.config().load();
    let correction = parse_correction(&cfg.experiments.default_correction);
    let allow_promotion = cfg.experiments.allow_promotion;
    drop(cfg);

    // Parse any supplied cells once (real measurements only; closed-vocab
    // validated). Kept in scope so both the record and in-memory-preview phases
    // use them.
    let parsed_cells: Option<Vec<CellMeasurement>> = match &params.cells {
        Some(cells_json) => {
            let mut cells = Vec::with_capacity(cells_json.len());
            for v in cells_json {
                cells.push(parse_cell(v)?);
            }
            Some(cells)
        }
        None => None,
    };

    // Record supplied cells through the existing measurement path, driven by the
    // `DatasetSource` harness (a `PrecomputedCells` replay of the real, imported
    // measurements — never fabricated).
    let mut recorded = serde_json::Value::Null;
    let mut preview = serde_json::Value::Null;
    if let Some(cells) = &parsed_cells {
        let (Some(eid), Some(hid)) = (experiment_id, hypothesis_id) else {
            return Err(McpError::invalid_params(
                "cells supplied but no experiment/hypothesis resolved; set open=true or pass \
                 experiment_id + hypothesis_id",
                None,
            ));
        };
        let runner = ContextTapeRunner::new(pool, eid, hid).with_correction(correction);
        let source = context_tape::PrecomputedCells::new(cells.clone());
        let grid = source.grid();
        let results = runner
            .record_from_source(&source, &grid)
            .await
            .map_err(|e| McpError::internal_error(format!("record cells: {e}"), None))?;
        let total: u64 = results.iter().map(|r| r.inserted_samples).sum();
        recorded = json!({
            "cells_recorded": results.len(),
            "samples_recorded": total,
        });
        // In-memory preview decision over the just-collected cells (no DB read) —
        // exercises the same frozen-criterion routing the persisted decide uses.
        let preview_decision = runner.evaluate_cells(cells);
        preview = json!({
            "accepted": preview_decision.accepted,
            "summary": preview_decision.summary(),
        });
    }

    // Authoritative decision against the FROZEN criterion over the PERSISTED
    // samples (per-clause routing). This is the decision promotion is gated on.
    let mut decision_json = serde_json::Value::Null;
    let mut promotion_json = serde_json::Value::Null;
    let want_decide = params
        .decide
        .unwrap_or(parsed_cells.is_some() || params.promote_to_obs.is_some());
    if want_decide {
        let (Some(eid), Some(hid)) = (experiment_id, hypothesis_id) else {
            return Err(McpError::invalid_params(
                "decide requested but no experiment/hypothesis resolved",
                None,
            ));
        };
        let runner = ContextTapeRunner::new(pool, eid, hid).with_correction(correction);
        let decision = runner
            .decide()
            .await
            .map_err(|e| McpError::internal_error(format!("decide: {e}"), None))?;
        decision_json = serde_json::to_value(&decision).unwrap_or(serde_json::Value::Null);

        // Promotion: default-OFF, verified-gated. Only a real (server-computed)
        // positive decision AND the explicit opt-in promote a memory observation.
        if let Some(obs_id) = params.promote_to_obs {
            let outcome = context_tape::promote_decision(pool, allow_promotion, &decision, obs_id)
                .await
                .map_err(|e| McpError::internal_error(format!("promote_decision: {e}"), None))?;
            promotion_json = json!({
                "target_observation_id": obs_id,
                "allow_promotion": allow_promotion,
                "decision_accepted": decision.accepted,
                "outcome": serde_json::to_value(&outcome).unwrap_or(serde_json::Value::Null),
            });
        }
        ctx.stats()
            .experiment_decisions_made
            .fetch_add(1, Ordering::Relaxed);
    }

    json_result(&json!({
        "preregistration": preregistration,
        "decided_by": nonblank_str(params.decided_by.as_deref()),
        "opened": opened,
        "recorded": recorded,
        "preview": preview,
        "decision": decision_json,
        "promotion": promotion_json,
    }))
}
