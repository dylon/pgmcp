//! Query layer for the scientific-experiment subsystem
//! (`crate::db::migrations::ensure_experiment_tables`).
//!
//! Plain `sqlx` free functions following the `queries.rs` idiom. JSONB columns
//! are bound as JSON **text** with a `$n::jsonb` cast and read back via a
//! `col::text` cast (the crate's sqlx build has no `json` feature). Embeddings
//! are `pgvector::Vector` (1024-d BGE-M3), bound as `Option<Vector>`.

use chrono::{DateTime, Utc};

use crate::experiment::vocab::ExperimentRunStatus;
use pgvector::Vector;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

// ============================================================================
// experiment_open
// ============================================================================

/// Insert a new experiment, returning its id. `hardware_json` /
/// `embedding` may be `"{}"` / `None`.
#[allow(clippy::too_many_arguments)]
pub async fn insert_experiment(
    pool: &PgPool,
    slug: &str,
    title: &str,
    question: &str,
    context: Option<&str>,
    kind: &str,
    project_id: Option<i32>,
    hardware_json: &str,
    git_ref: Option<&str>,
    plan_ref: Option<&str>,
    correction: &str,
    embedding: Option<Vector>,
) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let id = insert_experiment_in_tx(
        &mut tx,
        slug,
        title,
        question,
        context,
        kind,
        project_id,
        hardware_json,
        git_ref,
        plan_ref,
        correction,
        embedding,
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// Transactional variant of [`insert_experiment`].
#[allow(clippy::too_many_arguments)]
pub async fn insert_experiment_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    slug: &str,
    title: &str,
    question: &str,
    context: Option<&str>,
    kind: &str,
    project_id: Option<i32>,
    hardware_json: &str,
    git_ref: Option<&str>,
    plan_ref: Option<&str>,
    correction: &str,
    embedding: Option<Vector>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO experiments
            (slug, title, question, context, kind, project_id, hardware, git_ref,
             plan_ref, correction, embedding, embedding_signature)
         VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb, $8, $9, $10, $11,
                 CASE WHEN $11 IS NULL THEN 'bge-m3-v1' ELSE 'bge-m3-v1' END)
         RETURNING id",
    )
    .bind(slug)
    .bind(title)
    .bind(question)
    .bind(context)
    .bind(kind)
    .bind(project_id)
    .bind(hardware_json)
    .bind(git_ref)
    .bind(plan_ref)
    .bind(correction)
    .bind(embedding)
    .fetch_one(&mut **tx)
    .await
}

/// Insert a hypothesis with its pre-registered (frozen) acceptance criterion.
#[allow(clippy::too_many_arguments)]
pub async fn insert_experiment_hypothesis(
    pool: &PgPool,
    experiment_id: i64,
    statement: &str,
    primary_metric: &str,
    unit: Option<&str>,
    predicted_direction: &str,
    acceptance_criterion_json: &str,
    planned_n: Option<i32>,
    embedding: Option<Vector>,
) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let id = insert_experiment_hypothesis_in_tx(
        &mut tx,
        experiment_id,
        statement,
        primary_metric,
        unit,
        predicted_direction,
        acceptance_criterion_json,
        planned_n,
        embedding,
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// Transactional variant of [`insert_experiment_hypothesis`].
#[allow(clippy::too_many_arguments)]
pub async fn insert_experiment_hypothesis_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    experiment_id: i64,
    statement: &str,
    primary_metric: &str,
    unit: Option<&str>,
    predicted_direction: &str,
    acceptance_criterion_json: &str,
    planned_n: Option<i32>,
    embedding: Option<Vector>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO experiment_hypotheses
            (experiment_id, statement, primary_metric, unit, predicted_direction,
             acceptance_criterion, planned_n, embedding)
         VALUES ($1, $2, $3, $4, $5, $6::jsonb, $7, $8)
         RETURNING id",
    )
    .bind(experiment_id)
    .bind(statement)
    .bind(primary_metric)
    .bind(unit)
    .bind(predicted_direction)
    .bind(acceptance_criterion_json)
    .bind(planned_n)
    .bind(embedding)
    .fetch_one(&mut **tx)
    .await
}

/// Anchor an experiment to a code object. At least one of file/chunk/topic
/// must be `Some` (enforced by a table CHECK).
pub async fn insert_experiment_code_anchor(
    pool: &PgPool,
    experiment_id: i64,
    file_id: Option<i64>,
    chunk_id: Option<i64>,
    topic_id: Option<i64>,
    anchor_type: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO experiment_code_anchor
            (experiment_id, file_id, chunk_id, topic_id, anchor_type)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(experiment_id)
    .bind(file_id)
    .bind(chunk_id)
    .bind(topic_id)
    .bind(anchor_type)
    .execute(pool)
    .await?;
    Ok(())
}

/// Resolve a workspace-relative or absolute path to an `indexed_files.id`,
/// optionally scoped to a project. Used to turn `anchor_paths` into anchors.
pub async fn resolve_experiment_file_id(
    pool: &PgPool,
    project_id: Option<i32>,
    path: &str,
) -> Result<Option<i64>, sqlx::Error> {
    // Exact match first, then a suffix match (the agent may pass a
    // project-relative path while `indexed_files.path` is absolute).
    let exact: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM indexed_files
         WHERE path = $1 AND ($2::int IS NULL OR project_id = $2)
         LIMIT 1",
    )
    .bind(path)
    .bind(project_id)
    .fetch_optional(pool)
    .await?;
    if exact.is_some() {
        return Ok(exact);
    }
    sqlx::query_scalar(
        "SELECT id FROM indexed_files
         WHERE path LIKE '%' || $1 AND ($2::int IS NULL OR project_id = $2)
         ORDER BY length(path) ASC
         LIMIT 1",
    )
    .bind(path)
    .bind(project_id)
    .fetch_optional(pool)
    .await
}

// ============================================================================
// experiment_record_measurement
// ============================================================================

/// Find-or-create the run row for `(experiment_id, hypothesis_id, arm_label)`
/// and return its UUID. `IS NOT DISTINCT FROM` matches a NULL hypothesis_id.
/// On an existing run the JSONB/metadata columns are refreshed.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_experiment_run(
    pool: &PgPool,
    experiment_id: i64,
    hypothesis_id: Option<i64>,
    arm_label: &str,
    arm_kind: &str,
    command_spec_json: &str,
    run_plan_json: &str,
    host_meta_json: &str,
    git_ref: Option<&str>,
    runner: Option<&str>,
    seed: i64,
) -> Result<Uuid, sqlx::Error> {
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM experiment_runs
         WHERE experiment_id = $1 AND hypothesis_id IS NOT DISTINCT FROM $2 AND arm_label = $3
         LIMIT 1",
    )
    .bind(experiment_id)
    .bind(hypothesis_id)
    .bind(arm_label)
    .fetch_optional(pool)
    .await?;

    if let Some(id) = existing {
        sqlx::query(
            "UPDATE experiment_runs
             SET arm_kind = $2, command_spec = $3::jsonb,
                 run_plan = $4::jsonb, host_meta = $5::jsonb, git_ref = $6,
                 runner = $7, seed = $8, status = 'complete', finished_at = NOW()
             WHERE id = $1",
        )
        .bind(id)
        .bind(arm_kind)
        .bind(command_spec_json)
        .bind(run_plan_json)
        .bind(host_meta_json)
        .bind(git_ref)
        .bind(runner)
        .bind(seed)
        .execute(pool)
        .await?;
        return Ok(id);
    }

    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO experiment_runs
            (id, experiment_id, hypothesis_id, arm_label, arm_kind, command_spec,
             run_plan, host_meta, git_ref, runner, seed, status, started_at, finished_at)
         VALUES ($1, $2, $3, $4, $5, $6::jsonb, $7::jsonb,
                 $8::jsonb, $9, $10, $11, 'complete', NOW(), NOW())",
    )
    .bind(id)
    .bind(experiment_id)
    .bind(hypothesis_id)
    .bind(arm_label)
    .bind(arm_kind)
    .bind(command_spec_json)
    .bind(run_plan_json)
    .bind(host_meta_json)
    .bind(git_ref)
    .bind(runner)
    .bind(seed)
    .execute(pool)
    .await?;
    Ok(id)
}

/// Bulk-insert raw per-replicate samples for a run/arm/metric. `unit_keys`,
/// when present, must align with `samples` (paired-test keys); otherwise NULLs
/// are stored. Returns the number of rows inserted.
pub async fn insert_experiment_samples(
    pool: &PgPool,
    run_id: Uuid,
    arm: &str,
    metric_name: &str,
    samples: &[f64],
    unit_keys: Option<&[String]>,
    is_warmup: bool,
) -> Result<u64, sqlx::Error> {
    if samples.is_empty() {
        return Ok(0);
    }
    let indices: Vec<i32> = (0..samples.len() as i32).collect();
    let values: Vec<f64> = samples.to_vec();
    let keys: Vec<Option<String>> = match unit_keys {
        Some(k) => k.iter().map(|s| Some(s.clone())).collect(),
        None => vec![None; samples.len()],
    };
    let res = sqlx::query(
        "INSERT INTO experiment_samples
            (run_id, arm, metric_name, replicate_index, value, unit_key, is_warmup)
         SELECT $1, $2, $3, t.idx, t.val, t.uk, $7
         FROM UNNEST($4::int[], $5::double precision[], $6::text[]) AS t(idx, val, uk)",
    )
    .bind(run_id)
    .bind(arm)
    .bind(metric_name)
    .bind(&indices)
    .bind(&values)
    .bind(&keys)
    .bind(is_warmup)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Complete write payload for `experiment_record_measurement`.
#[allow(clippy::too_many_arguments)]
pub struct RecordExperimentMeasurement<'a> {
    pub experiment_id: i64,
    pub hypothesis_id: Option<i64>,
    pub arm_label: &'a str,
    pub arm_kind: &'a str,
    pub command_spec_json: &'a str,
    pub run_plan_json: &'a str,
    pub host_meta_json: &'a str,
    pub git_ref: Option<&'a str>,
    pub runner: Option<&'a str>,
    pub seed: i64,
    pub metric_name: &'a str,
    pub samples: &'a [f64],
    pub unit_keys: Option<&'a [String]>,
    pub is_warmup: bool,
}

/// Result of the atomic measurement write.
pub struct RecordedExperimentMeasurement {
    pub run_id: Uuid,
    pub inserted_samples: u64,
}

/// Atomically find/create the run, append samples, and mark the experiment as
/// measuring. The transaction takes exactly one advisory lock keyed by the run
/// identity, which serializes concurrent NULL-hypothesis upserts without
/// introducing a lock-order cycle.
pub async fn record_experiment_measurement(
    pool: &PgPool,
    r: RecordExperimentMeasurement<'_>,
) -> Result<RecordedExperimentMeasurement, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let res = async {
        let (lock_a, lock_b) =
            experiment_run_advisory_lock_key(r.experiment_id, r.hypothesis_id, r.arm_label);
        sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
            .bind(lock_a)
            .bind(lock_b)
            .execute(&mut *tx)
            .await?;

        let existing: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM experiment_runs
             WHERE experiment_id = $1 AND hypothesis_id IS NOT DISTINCT FROM $2 AND arm_label = $3
             LIMIT 1",
        )
        .bind(r.experiment_id)
        .bind(r.hypothesis_id)
        .bind(r.arm_label)
        .fetch_optional(&mut *tx)
        .await?;

        let run_id = if let Some(id) = existing {
            sqlx::query(
                "UPDATE experiment_runs
                 SET arm_kind = $2, command_spec = $3::jsonb,
                     run_plan = $4::jsonb, host_meta = $5::jsonb, git_ref = $6,
                     runner = $7, seed = $8, status = 'complete', finished_at = NOW()
                 WHERE id = $1",
            )
            .bind(id)
            .bind(r.arm_kind)
            .bind(r.command_spec_json)
            .bind(r.run_plan_json)
            .bind(r.host_meta_json)
            .bind(r.git_ref)
            .bind(r.runner)
            .bind(r.seed)
            .execute(&mut *tx)
            .await?;
            id
        } else {
            let id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO experiment_runs
                    (id, experiment_id, hypothesis_id, arm_label, arm_kind, command_spec,
                     run_plan, host_meta, git_ref, runner, seed, status, started_at, finished_at)
                 VALUES ($1, $2, $3, $4, $5, $6::jsonb, $7::jsonb,
                         $8::jsonb, $9, $10, $11, 'complete', NOW(), NOW())",
            )
            .bind(id)
            .bind(r.experiment_id)
            .bind(r.hypothesis_id)
            .bind(r.arm_label)
            .bind(r.arm_kind)
            .bind(r.command_spec_json)
            .bind(r.run_plan_json)
            .bind(r.host_meta_json)
            .bind(r.git_ref)
            .bind(r.runner)
            .bind(r.seed)
            .execute(&mut *tx)
            .await?;
            id
        };

        let indices: Vec<i32> = (0..r.samples.len() as i32).collect();
        let values: Vec<f64> = r.samples.to_vec();
        let keys: Vec<Option<String>> = match r.unit_keys {
            Some(k) => k.iter().map(|s| Some(s.clone())).collect(),
            None => vec![None; r.samples.len()],
        };
        let inserted_samples = if r.samples.is_empty() {
            0
        } else {
            sqlx::query(
                "INSERT INTO experiment_samples
                    (run_id, arm, metric_name, replicate_index, value, unit_key, is_warmup)
                 SELECT $1, $2, $3, t.idx, t.val, t.uk, $7
                 FROM UNNEST($4::int[], $5::double precision[], $6::text[]) AS t(idx, val, uk)",
            )
            .bind(run_id)
            .bind(r.arm_label)
            .bind(r.metric_name)
            .bind(&indices)
            .bind(&values)
            .bind(&keys)
            .bind(r.is_warmup)
            .execute(&mut *tx)
            .await?
            .rows_affected()
        };

        sqlx::query(
            "UPDATE experiments SET status = 'measuring', updated_at = NOW()
             WHERE id = $1",
        )
        .bind(r.experiment_id)
        .execute(&mut *tx)
        .await?;

        Ok::<RecordedExperimentMeasurement, sqlx::Error>(RecordedExperimentMeasurement {
            run_id,
            inserted_samples,
        })
    }
    .await;

    match res {
        Ok(recorded) => {
            tx.commit().await?;
            Ok(recorded)
        }
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

fn experiment_run_advisory_lock_key(
    experiment_id: i64,
    hypothesis_id: Option<i64>,
    arm_label: &str,
) -> (i32, i32) {
    let mut high = experiment_id as u64;
    high ^= (hypothesis_id.unwrap_or(-1) as u64).rotate_left(17);

    let mut low = 0x811c9dc5u32;
    for byte in arm_label.as_bytes() {
        low ^= u32::from(*byte);
        low = low.wrapping_mul(0x01000193);
    }

    (high as u32 as i32, low as i32)
}

/// Set an experiment's lifecycle status.
pub async fn set_experiment_status(
    pool: &PgPool,
    experiment_id: i64,
    status: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE experiments SET status = $2, updated_at = NOW() WHERE id = $1")
        .bind(experiment_id)
        .bind(status)
        .execute(pool)
        .await?;
    Ok(())
}

// ============================================================================
// Read helpers (shared by decide / get / record validation)
// ============================================================================

/// A hypothesis row with its frozen criterion (as JSON text).
#[derive(Debug, Clone)]
pub struct ExperimentHypothesisRow {
    pub id: i64,
    pub experiment_id: i64,
    pub statement: String,
    pub primary_metric: String,
    pub unit: Option<String>,
    pub predicted_direction: String,
    pub acceptance_criterion_json: String,
    pub criterion_locked_at: DateTime<Utc>,
    pub planned_n: Option<i32>,
    pub verdict: String,
}

/// Column tuple for an `experiment_hypotheses` row as selected by
/// `get_experiment_hypothesis` / `list_experiment_hypotheses`
/// (id, experiment_id, statement, primary_metric, unit, predicted_direction,
/// acceptance_criterion, criterion_locked_at, planned_n, verdict).
type HypothesisRowTuple = (
    i64,
    i64,
    String,
    String,
    Option<String>,
    String,
    String,
    DateTime<Utc>,
    Option<i32>,
    String,
);

/// Load a hypothesis (criterion read via `::text`). `None` if absent.
pub async fn get_experiment_hypothesis(
    pool: &PgPool,
    hypothesis_id: i64,
) -> Result<Option<ExperimentHypothesisRow>, sqlx::Error> {
    let row: Option<HypothesisRowTuple> = sqlx::query_as(
        "SELECT id, experiment_id, statement, primary_metric, unit,
                predicted_direction::text, acceptance_criterion::text,
                criterion_locked_at, planned_n, verdict::text
         FROM experiment_hypotheses WHERE id = $1",
    )
    .bind(hypothesis_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| ExperimentHypothesisRow {
        id: r.0,
        experiment_id: r.1,
        statement: r.2,
        primary_metric: r.3,
        unit: r.4,
        predicted_direction: r.5,
        acceptance_criterion_json: r.6,
        criterion_locked_at: r.7,
        planned_n: r.8,
        verdict: r.9,
    }))
}

/// Load the non-warm-up samples for one arm/metric of a hypothesis, ordered by
/// `unit_key` then `replicate_index` (so paired tests align across arms).
/// Returns `(value, unit_key)` tuples.
pub async fn load_experiment_samples(
    pool: &PgPool,
    experiment_id: i64,
    hypothesis_id: Option<i64>,
    arm_label: &str,
    metric_name: &str,
) -> Result<Vec<(f64, Option<String>)>, sqlx::Error> {
    // ANTI-TAMPER (Thread 5b): only runs usable in a decision contribute
    // samples — an `invalid`/`superseded`/`pending` run is excluded so it cannot
    // silently skew the verdict. The status list is derived from
    // `ExperimentRunStatus::usable_in_decision` so the gate and the vocabulary
    // cannot drift.
    let sql = format!(
        "SELECT s.value, s.unit_key
         FROM experiment_samples s
         JOIN experiment_runs r ON r.id = s.run_id
         WHERE r.experiment_id = $1
           AND r.hypothesis_id IS NOT DISTINCT FROM $2
           AND s.arm = $3
           AND s.metric_name = $4
           AND NOT s.is_warmup
           AND r.status IN ({usable})
         ORDER BY s.unit_key NULLS FIRST, s.replicate_index",
        usable = crate::experiment::vocab::ExperimentRunStatus::usable_in_decision_sql_list(),
    );
    sqlx::query_as::<_, (f64, Option<String>)>(sqlx::AssertSqlSafe(sql))
        .bind(experiment_id)
        .bind(hypothesis_id)
        .bind(arm_label)
        .bind(metric_name)
        .fetch_all(pool)
        .await
}

// ============================================================================
// Thread 5b — experiment-API hardening: run finalize/status audit + tamper-
// evident samples digest + paired-corpus 2×2 counts. All anti-tampering machinery
// stays inside the experiment subsystem (it never crosses the work-item tracker's
// →verified boundary).
// ============================================================================

/// Counts of a paired-corpus 2×2 (same test cases scored by both arms).
#[derive(Debug, Clone, Copy)]
pub struct PairedBinaryCounts {
    pub both_correct: i64,
    pub control_only: i64,
    pub treatment_only: i64,
    pub both_wrong: i64,
}

/// Result of finalizing a measurement run.
pub struct RunFinalizeResult {
    pub samples_digest: String,
    pub sample_count: i64,
}

/// Outcome of an audited run-status change.
pub struct RunStatusChange {
    pub old_status: String,
    pub new_status: String,
    /// Decision ids re-opened because they had used this (now-excluded) run.
    pub reopened_decisions: Vec<i64>,
}

/// One run's audit-visible state, for `experiment_get`.
pub struct RunOverview {
    pub run_id: Uuid,
    pub arm_label: String,
    pub arm_kind: String,
    pub status: String,
    pub sample_count: i64,
    pub samples_digest: Option<String>,
    pub status_reason: Option<String>,
    pub finalized_at: Option<DateTime<Utc>>,
}

/// Column tuple for a run row as selected by [`experiment_runs_overview`]
/// (id, arm_label, arm_kind, status, non-warmup sample_count, samples_digest,
/// status_reason, finalized_at). Named per the `HypothesisRowTuple` idiom so the
/// `sqlx::query_as` target type stays readable.
type RunOverviewTuple = (
    Uuid,
    String,
    String,
    String,
    i64,
    Option<String>,
    Option<String>,
    Option<DateTime<Utc>>,
);

/// Tamper-evident SHA-256 over a run's ordered raw samples. Deterministic: rows
/// are ordered canonically and values are hashed bit-exactly (`f64::to_bits`), so
/// any later mutation of the samples changes the digest. Snapshotted at finalize
/// and on a decision, so excluded/altered data is detectable after the fact.
pub async fn compute_run_samples_digest(
    pool: &PgPool,
    run_id: Uuid,
) -> Result<String, sqlx::Error> {
    use sha2::{Digest, Sha256};
    let rows: Vec<(String, String, i32, f64, Option<String>, bool)> = sqlx::query_as(
        "SELECT metric_name, arm, replicate_index, value, unit_key, is_warmup
         FROM experiment_samples WHERE run_id = $1
         ORDER BY metric_name, arm, unit_key NULLS FIRST, replicate_index, value",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?;
    let mut hasher = Sha256::new();
    hasher.update((rows.len() as u64).to_le_bytes());
    for (metric, arm, idx, value, unit_key, is_warmup) in &rows {
        hasher.update(
            format!(
                "{metric}\u{1f}{arm}\u{1f}{}\u{1f}{idx}\u{1f}{is_warmup}\u{1f}{:016x}\n",
                unit_key.as_deref().unwrap_or(""),
                value.to_bits(),
            )
            .as_bytes(),
        );
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

/// Append one row to the immutable `experiment_run_status_audit` trail.
async fn append_run_status_audit(
    conn: &mut sqlx::PgConnection,
    run_id: Uuid,
    old_status: Option<&str>,
    new_status: &str,
    reason: &str,
    changed_by: &str,
    decision_id: Option<i64>,
    samples_digest: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO experiment_run_status_audit
            (run_id, old_status, new_status, reason, changed_by, decision_id, samples_digest)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(run_id)
    .bind(old_status)
    .bind(new_status)
    .bind(reason)
    .bind(changed_by)
    .bind(decision_id)
    .bind(samples_digest)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Seal a run for use in decisions: compute + store its samples digest, set
/// `status='finalized'`, and append to the audit trail. Idempotent-ish (re-running
/// recomputes the digest and re-stamps). Returns the digest + non-warmup count.
pub async fn finalize_experiment_run(
    pool: &PgPool,
    run_id: Uuid,
    changed_by: &str,
    reason: &str,
) -> Result<RunFinalizeResult, sqlx::Error> {
    let samples_digest = compute_run_samples_digest(pool, run_id).await?;
    let mut tx = pool.begin().await?;
    let old_status: Option<String> =
        sqlx::query_scalar("SELECT status FROM experiment_runs WHERE id = $1 FOR UPDATE")
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(old_status) = old_status else {
        return Err(sqlx::Error::RowNotFound);
    };
    let sample_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM experiment_samples WHERE run_id = $1 AND NOT is_warmup",
    )
    .bind(run_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE experiment_runs
         SET status = 'finalized', samples_digest = $2, finalized_at = NOW(),
             status_reason = $3, status_changed_by = $4, status_changed_at = NOW()
         WHERE id = $1",
    )
    .bind(run_id)
    .bind(&samples_digest)
    .bind(reason)
    .bind(changed_by)
    .execute(&mut *tx)
    .await?;
    append_run_status_audit(
        &mut tx,
        run_id,
        Some(&old_status),
        "finalized",
        reason,
        changed_by,
        None,
        Some(&samples_digest),
    )
    .await?;
    tx.commit().await?;
    Ok(RunFinalizeResult {
        samples_digest,
        sample_count,
    })
}

/// Audited exclusion of a run from decisions (`invalid`/`superseded`). THE
/// anti-cherry-pick guardrail: any rendered decision that USED this run is
/// re-opened (its hypothesis verdict reverts to `pending`), so excluding data
/// after a decision can never silently keep the favourable verdict — it forces a
/// re-decision. Every change is appended to the immutable audit trail with the
/// reason + actor + the samples digest at the time. The caller (tool layer)
/// validates that `new_status ∈ {invalid, superseded}` and `reason` is non-empty.
pub async fn set_experiment_run_status(
    pool: &PgPool,
    run_id: Uuid,
    new_status: ExperimentRunStatus,
    reason: &str,
    changed_by: &str,
) -> Result<RunStatusChange, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT status, samples_digest FROM experiment_runs WHERE id = $1 FOR UPDATE",
    )
    .bind(run_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((old_status, samples_digest)) = row else {
        return Err(sqlx::Error::RowNotFound);
    };
    // Decisions that consumed this run (as control or treatment) must be re-opened.
    let affected: Vec<(i64,)> = sqlx::query_as(
        "SELECT id FROM experiment_results WHERE control_run_id = $1 OR treatment_run_id = $1",
    )
    .bind(run_id)
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE experiment_runs
         SET status = $2, status_reason = $3, status_changed_by = $4, status_changed_at = NOW()
         WHERE id = $1",
    )
    .bind(run_id)
    .bind(new_status.as_str())
    .bind(reason)
    .bind(changed_by)
    .execute(&mut *tx)
    .await?;
    let mut reopened_decisions = Vec::with_capacity(affected.len());
    for (decision_id,) in &affected {
        sqlx::query(
            "UPDATE experiment_hypotheses h
             SET verdict = 'pending'
             FROM experiment_results r
             WHERE r.id = $1 AND h.id = r.hypothesis_id",
        )
        .bind(decision_id)
        .execute(&mut *tx)
        .await?;
        append_run_status_audit(
            &mut tx,
            run_id,
            Some(&old_status),
            new_status.as_str(),
            reason,
            changed_by,
            Some(*decision_id),
            samples_digest.as_deref(),
        )
        .await?;
        reopened_decisions.push(*decision_id);
    }
    if affected.is_empty() {
        append_run_status_audit(
            &mut tx,
            run_id,
            Some(&old_status),
            new_status.as_str(),
            reason,
            changed_by,
            None,
            samples_digest.as_deref(),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(RunStatusChange {
        old_status,
        new_status: new_status.as_str().to_string(),
        reopened_decisions,
    })
}

/// Upsert the paired-corpus 2×2 counts for a `(experiment, hypothesis, metric)`.
/// Returns the row id.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_paired_binary_counts(
    pool: &PgPool,
    experiment_id: i64,
    hypothesis_id: Option<i64>,
    metric_name: &str,
    control_run_id: Option<Uuid>,
    treatment_run_id: Option<Uuid>,
    counts: PairedBinaryCounts,
    source: Option<&str>,
    detail_json: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO experiment_paired_binary
            (experiment_id, hypothesis_id, metric_name, control_run_id, treatment_run_id,
             both_correct, control_only, treatment_only, both_wrong, source, detail, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::jsonb, NOW())
         ON CONFLICT (experiment_id, hypothesis_id, metric_name) DO UPDATE SET
            control_run_id = EXCLUDED.control_run_id,
            treatment_run_id = EXCLUDED.treatment_run_id,
            both_correct = EXCLUDED.both_correct,
            control_only = EXCLUDED.control_only,
            treatment_only = EXCLUDED.treatment_only,
            both_wrong = EXCLUDED.both_wrong,
            source = EXCLUDED.source,
            detail = EXCLUDED.detail,
            updated_at = NOW()
         RETURNING id",
    )
    .bind(experiment_id)
    .bind(hypothesis_id)
    .bind(metric_name)
    .bind(control_run_id)
    .bind(treatment_run_id)
    .bind(counts.both_correct)
    .bind(counts.control_only)
    .bind(counts.treatment_only)
    .bind(counts.both_wrong)
    .bind(source)
    .bind(detail_json)
    .fetch_one(pool)
    .await
}

/// Load the paired-corpus 2×2 for a `(experiment, hypothesis, metric)`, if any.
pub async fn load_paired_binary_counts(
    pool: &PgPool,
    experiment_id: i64,
    hypothesis_id: Option<i64>,
    metric_name: &str,
) -> Result<Option<PairedBinaryCounts>, sqlx::Error> {
    let row: Option<(i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT both_correct, control_only, treatment_only, both_wrong
         FROM experiment_paired_binary
         WHERE experiment_id = $1 AND hypothesis_id IS NOT DISTINCT FROM $2 AND metric_name = $3",
    )
    .bind(experiment_id)
    .bind(hypothesis_id)
    .bind(metric_name)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(
        |(both_correct, control_only, treatment_only, both_wrong)| PairedBinaryCounts {
            both_correct,
            control_only,
            treatment_only,
            both_wrong,
        },
    ))
}

/// Audit-visible overview of every run of an experiment (status + sample counts +
/// digest), for `experiment_get`.
pub async fn experiment_runs_overview(
    pool: &PgPool,
    experiment_id: i64,
) -> Result<Vec<RunOverview>, sqlx::Error> {
    let rows: Vec<RunOverviewTuple> = sqlx::query_as(
        "SELECT r.id, r.arm_label, r.arm_kind, r.status,
                (SELECT count(*) FROM experiment_samples s WHERE s.run_id = r.id AND NOT s.is_warmup),
                r.samples_digest, r.status_reason, r.finalized_at
         FROM experiment_runs r
         WHERE r.experiment_id = $1
         ORDER BY r.created_at, r.arm_label",
    )
    .bind(experiment_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                run_id,
                arm_label,
                arm_kind,
                status,
                sample_count,
                samples_digest,
                status_reason,
                finalized_at,
            )| {
                RunOverview {
                    run_id,
                    arm_label,
                    arm_kind,
                    status,
                    sample_count,
                    samples_digest,
                    status_reason,
                    finalized_at,
                }
            },
        )
        .collect())
}

/// The earliest non-warm-up sample time for a hypothesis (anti-p-hacking guard
/// in `experiment_decide`: the criterion must predate the first measurement).
pub async fn earliest_measurement_time(
    pool: &PgPool,
    hypothesis_id: i64,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT MIN(s.recorded_at)
         FROM experiment_samples s
         JOIN experiment_runs r ON r.id = s.run_id
         WHERE r.hypothesis_id = $1 AND NOT s.is_warmup",
    )
    .bind(hypothesis_id)
    .fetch_one(pool)
    .await
}

/// Find a run id by `(experiment_id, hypothesis_id, arm_label)` to record on a
/// decision's control/treatment pointers.
pub async fn find_experiment_run_id(
    pool: &PgPool,
    experiment_id: i64,
    hypothesis_id: Option<i64>,
    arm_label: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT id FROM experiment_runs
         WHERE experiment_id = $1 AND hypothesis_id IS NOT DISTINCT FROM $2 AND arm_label = $3
         LIMIT 1",
    )
    .bind(experiment_id)
    .bind(hypothesis_id)
    .bind(arm_label)
    .fetch_optional(pool)
    .await
}

// ============================================================================
// experiment_decide
// ============================================================================

/// Persist a statistical decision, returning its id. NaN p-values / effect
/// sizes (non-NHST evidence) are stored as Postgres `'NaN'::float8`.
#[allow(clippy::too_many_arguments)]
pub struct InsertExperimentResult<'a> {
    pub experiment_id: i64,
    pub hypothesis_id: i64,
    pub test_type: &'a str,
    pub metric_name: &'a str,
    pub control_run_id: Option<Uuid>,
    pub treatment_run_id: Option<Uuid>,
    pub statistic: Option<f64>,
    pub df: Option<f64>,
    pub p_value: Option<f64>,
    pub effect_size: Option<f64>,
    pub effect_size_kind: Option<&'a str>,
    pub ci_low: Option<f64>,
    pub ci_high: Option<f64>,
    pub ci_level: Option<f64>,
    pub verdict: &'a str,
    pub accepted: bool,
    pub correction: Option<&'a str>,
    pub criterion_snapshot_json: &'a str,
    pub test_result_json: &'a str,
    pub rationale: Option<&'a str>,
    pub decided_by: Option<&'a str>,
    pub embedding: Option<Vector>,
    pub observation_id: Option<i64>,
}

/// Insert an `experiment_results` row from the bundled fields.
pub async fn insert_experiment_result(
    pool: &PgPool,
    r: InsertExperimentResult<'_>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO experiment_results
            (experiment_id, hypothesis_id, test_type, metric_name, control_run_id,
             treatment_run_id, statistic, df, p_value, effect_size, effect_size_kind,
             ci_low, ci_high, ci_level, verdict, accepted, correction,
             criterion_snapshot, test_result, rationale, decided_by, embedding,
             observation_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                 $15, $16, $17, $18::jsonb, $19::jsonb, $20, $21, $22, $23)
         RETURNING id",
    )
    .bind(r.experiment_id)
    .bind(r.hypothesis_id)
    .bind(r.test_type)
    .bind(r.metric_name)
    .bind(r.control_run_id)
    .bind(r.treatment_run_id)
    .bind(r.statistic)
    .bind(r.df)
    .bind(r.p_value)
    .bind(r.effect_size)
    .bind(r.effect_size_kind)
    .bind(r.ci_low)
    .bind(r.ci_high)
    .bind(r.ci_level)
    .bind(r.verdict)
    .bind(r.accepted)
    .bind(r.correction)
    .bind(r.criterion_snapshot_json)
    .bind(r.test_result_json)
    .bind(r.rationale)
    .bind(r.decided_by)
    .bind(r.embedding)
    .bind(r.observation_id)
    .fetch_one(pool)
    .await
}

/// Atomically persist the decision row and publish the experiment/hypothesis
/// status updates that make the decision visible.
pub async fn insert_experiment_decision(
    pool: &PgPool,
    r: InsertExperimentResult<'_>,
) -> Result<i64, sqlx::Error> {
    let experiment_id = r.experiment_id;
    let hypothesis_id = r.hypothesis_id;
    let verdict = r.verdict.to_string();
    let observation_id = r.observation_id;

    let mut tx = pool.begin().await?;
    let result_id: i64 = sqlx::query_scalar(
        "INSERT INTO experiment_results
            (experiment_id, hypothesis_id, test_type, metric_name, control_run_id,
             treatment_run_id, statistic, df, p_value, effect_size, effect_size_kind,
             ci_low, ci_high, ci_level, verdict, accepted, correction,
             criterion_snapshot, test_result, rationale, decided_by, embedding,
             observation_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                 $15, $16, $17, $18::jsonb, $19::jsonb, $20, $21, $22, $23)
         RETURNING id",
    )
    .bind(r.experiment_id)
    .bind(r.hypothesis_id)
    .bind(r.test_type)
    .bind(r.metric_name)
    .bind(r.control_run_id)
    .bind(r.treatment_run_id)
    .bind(r.statistic)
    .bind(r.df)
    .bind(r.p_value)
    .bind(r.effect_size)
    .bind(r.effect_size_kind)
    .bind(r.ci_low)
    .bind(r.ci_high)
    .bind(r.ci_level)
    .bind(r.verdict)
    .bind(r.accepted)
    .bind(r.correction)
    .bind(r.criterion_snapshot_json)
    .bind(r.test_result_json)
    .bind(r.rationale)
    .bind(r.decided_by)
    .bind(r.embedding)
    .bind(r.observation_id)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query("UPDATE experiment_hypotheses SET verdict = $2 WHERE id = $1")
        .bind(hypothesis_id)
        .bind(&verdict)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE experiments SET status = $2, updated_at = NOW() WHERE id = $1")
        .bind(experiment_id)
        .bind("decided")
        .execute(&mut *tx)
        .await?;
    if let Some(oid) = observation_id {
        sqlx::query("UPDATE experiment_results SET observation_id = $2 WHERE id = $1")
            .bind(result_id)
            .bind(oid)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE experiments SET observation_id = $2, updated_at = NOW() WHERE id = $1")
            .bind(experiment_id)
            .bind(oid)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(result_id)
}

/// Set a hypothesis's verdict.
pub async fn set_hypothesis_verdict(
    pool: &PgPool,
    hypothesis_id: i64,
    verdict: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE experiment_hypotheses SET verdict = $2 WHERE id = $1")
        .bind(hypothesis_id)
        .bind(verdict)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record the mirror observation id on an experiment (after the dual-write).
pub async fn set_experiment_observation_id(
    pool: &PgPool,
    experiment_id: i64,
    observation_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE experiments SET observation_id = $2, updated_at = NOW() WHERE id = $1")
        .bind(experiment_id)
        .bind(observation_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record the mirror observation id on a decision.
pub async fn set_result_observation_id(
    pool: &PgPool,
    result_id: i64,
    observation_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE experiment_results SET observation_id = $2 WHERE id = $1")
        .bind(result_id)
        .bind(observation_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ============================================================================
// experiment_search / get / list / timeline
// ============================================================================

/// A cross-project search hit.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExperimentSearchHit {
    pub id: i64,
    pub slug: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub project: Option<String>,
    pub similarity: f64,
    pub verdict: Option<String>,
    pub p_value: Option<f64>,
}

/// Vector search over experiment embeddings (the title‖question‖context
/// signal). `project_id = None` ⇒ cross-project. Returns each experiment with
/// its first hypothesis's verdict and latest decision p-value.
pub async fn experiment_search_vector(
    pool: &PgPool,
    query_embedding: &Vector,
    project_id: Option<i32>,
    kind: Option<&str>,
    verdict: Option<&str>,
    limit: i64,
) -> Result<Vec<ExperimentSearchHit>, sqlx::Error> {
    sqlx::query_as::<_, ExperimentSearchHit>(
        "SELECT e.id, e.slug, e.title, e.kind::text AS kind, e.status::text AS status,
                p.name AS project,
                1.0 - (e.embedding <=> $1) AS similarity,
                (SELECT h.verdict::text FROM experiment_hypotheses h
                  WHERE h.experiment_id = e.id AND h.valid_to IS NULL
                  ORDER BY h.id LIMIT 1) AS verdict,
                (SELECT r.p_value FROM experiment_results r
                  WHERE r.experiment_id = e.id ORDER BY r.id DESC LIMIT 1) AS p_value
         FROM experiments e
         LEFT JOIN projects p ON p.id = e.project_id
         WHERE e.valid_to IS NULL AND e.embedding IS NOT NULL
           AND ($2::int IS NULL OR e.project_id = $2)
           AND ($3::text IS NULL OR e.kind::text = $3)
           AND ($4::text IS NULL OR EXISTS (
                 SELECT 1 FROM experiment_hypotheses h2
                 WHERE h2.experiment_id = e.id
                   AND h2.valid_to IS NULL
                   AND h2.verdict::text = $4))
         ORDER BY e.embedding <=> $1
         LIMIT $5",
    )
    .bind(query_embedding)
    .bind(project_id)
    .bind(kind)
    .bind(verdict)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Full-text fallback when no query embedding is available (or to fuse).
pub async fn experiment_search_fts(
    pool: &PgPool,
    query: &str,
    project_id: Option<i32>,
    kind: Option<&str>,
    verdict: Option<&str>,
    limit: i64,
) -> Result<Vec<ExperimentSearchHit>, sqlx::Error> {
    sqlx::query_as::<_, ExperimentSearchHit>(
        "SELECT e.id, e.slug, e.title, e.kind::text AS kind, e.status::text AS status,
                p.name AS project,
                ts_rank(to_tsvector('english', coalesce(e.title,'') || ' ' ||
                        coalesce(e.question,'') || ' ' || coalesce(e.context,'')),
                        plainto_tsquery('english', $1))::float8 AS similarity,
                (SELECT h.verdict::text FROM experiment_hypotheses h
                  WHERE h.experiment_id = e.id AND h.valid_to IS NULL
                  ORDER BY h.id LIMIT 1) AS verdict,
                (SELECT r.p_value FROM experiment_results r
                  WHERE r.experiment_id = e.id ORDER BY r.id DESC LIMIT 1) AS p_value
         FROM experiments e
         LEFT JOIN projects p ON p.id = e.project_id
         WHERE e.valid_to IS NULL
           AND ($2::int IS NULL OR e.project_id = $2)
           AND ($3::text IS NULL OR e.kind::text = $3)
           AND ($4::text IS NULL OR EXISTS (
                 SELECT 1 FROM experiment_hypotheses h2
                 WHERE h2.experiment_id = e.id
                   AND h2.valid_to IS NULL
                   AND h2.verdict::text = $4))
           AND to_tsvector('english', coalesce(e.title,'') || ' ' ||
                 coalesce(e.question,'') || ' ' || coalesce(e.context,''))
               @@ plainto_tsquery('english', $1)
         ORDER BY similarity DESC
         LIMIT $5",
    )
    .bind(query)
    .bind(project_id)
    .bind(kind)
    .bind(verdict)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// The core experiment row (for `experiment_get` / `experiment_timeline`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExperimentCoreRow {
    pub id: i64,
    pub slug: String,
    pub title: String,
    pub question: String,
    pub context: Option<String>,
    pub kind: String,
    pub status: String,
    pub project: Option<String>,
    pub git_ref: Option<String>,
    pub plan_ref: Option<String>,
    pub correction: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Fetch the core experiment row by id or slug (id wins if both given).
pub async fn get_experiment_core(
    pool: &PgPool,
    id: Option<i64>,
    slug: Option<&str>,
) -> Result<Option<ExperimentCoreRow>, sqlx::Error> {
    sqlx::query_as::<_, ExperimentCoreRow>(
        "SELECT e.id, e.slug, e.title, e.question, e.context, e.kind::text AS kind,
                e.status::text AS status, p.name AS project, e.git_ref, e.plan_ref,
                e.correction, e.created_at, e.updated_at
         FROM experiments e
         LEFT JOIN projects p ON p.id = e.project_id
         WHERE e.valid_to IS NULL
           AND (($1::bigint IS NOT NULL AND e.id = $1) OR ($1::bigint IS NULL AND e.slug = $2))
         ORDER BY e.id DESC
         LIMIT 1",
    )
    .bind(id)
    .bind(slug)
    .fetch_optional(pool)
    .await
}

/// The mutable-assignment projection of an experiment used by the operator
/// `PATCH /api/experiments/{slug}` write path: the join key + the three fields
/// the console can edit (project / status / title). `project` is the joined
/// `projects.name` (for display), `project_id` the raw FK (for the picker).
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ExperimentAssignmentRow {
    pub id: i64,
    pub slug: String,
    pub title: String,
    pub status: String,
    pub project_id: Option<i32>,
    pub project: Option<String>,
}

/// Lock + read the current (`valid_to IS NULL`) experiment version by slug for
/// an operator assignment write, returning its editable projection (or `None`
/// when the slug is unknown). `FOR UPDATE OF e` locks only the `experiments`
/// row (the left-joined `projects` side cannot be locked) so a concurrent
/// operator edit serializes behind this one.
pub async fn get_experiment_assignment_for_update_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    slug: &str,
) -> Result<Option<ExperimentAssignmentRow>, sqlx::Error> {
    sqlx::query_as::<_, ExperimentAssignmentRow>(
        "SELECT e.id, e.slug, e.title, e.status::text AS status, e.project_id, p.name AS project
         FROM experiments e
         LEFT JOIN projects p ON p.id = e.project_id
         WHERE e.slug = $1 AND e.valid_to IS NULL
         ORDER BY e.id DESC
         LIMIT 1
         FOR UPDATE OF e",
    )
    .bind(slug)
    .fetch_optional(&mut **tx)
    .await
}

/// Apply an operator assignment edit to the current experiment version and
/// return its updated projection. Each field is independently optional:
///
/// - `project_touched=false` leaves `project_id` unchanged; `project_touched=
///   true` SETS it to `project_id` (which may be `None` to CLEAR — a plain
///   COALESCE cannot express "set to NULL", hence the explicit boolean flag).
/// - `status` / `title` use COALESCE, so `None` leaves them unchanged (both are
///   NOT NULL columns, so there is no "clear" case for them).
///
/// The `project` name in the returned row is resolved via a correlated
/// subquery against the post-update `project_id`, so a single statement yields
/// the canonical joined view.
pub async fn update_experiment_assignment_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: i64,
    project_touched: bool,
    project_id: Option<i32>,
    status: Option<&str>,
    title: Option<&str>,
) -> Result<Option<ExperimentAssignmentRow>, sqlx::Error> {
    sqlx::query_as::<_, ExperimentAssignmentRow>(
        "UPDATE experiments e SET
             project_id = CASE WHEN $2 THEN $3 ELSE e.project_id END,
             status     = COALESCE($4, e.status),
             title      = COALESCE($5, e.title),
             updated_at = NOW()
         WHERE e.id = $1 AND e.valid_to IS NULL
         RETURNING e.id, e.slug, e.title, e.status::text AS status, e.project_id,
                   (SELECT p.name FROM projects p WHERE p.id = e.project_id) AS project",
    )
    .bind(id)
    .bind(project_touched)
    .bind(project_id)
    .bind(status)
    .bind(title)
    .fetch_optional(&mut **tx)
    .await
}

/// A decision row for `experiment_get`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExperimentResultRow {
    pub id: i64,
    pub hypothesis_id: i64,
    pub test_type: String,
    pub metric_name: String,
    pub statistic: Option<f64>,
    pub p_value: Option<f64>,
    pub effect_size: Option<f64>,
    pub ci_low: Option<f64>,
    pub ci_high: Option<f64>,
    pub verdict: String,
    pub accepted: bool,
    pub rationale: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// All decisions for an experiment, newest first.
pub async fn list_experiment_results(
    pool: &PgPool,
    experiment_id: i64,
) -> Result<Vec<ExperimentResultRow>, sqlx::Error> {
    sqlx::query_as::<_, ExperimentResultRow>(
        "SELECT id, hypothesis_id, test_type, metric_name, statistic, p_value,
                effect_size, ci_low, ci_high, verdict::text AS verdict, accepted,
                rationale, created_at
         FROM experiment_results WHERE experiment_id = $1 ORDER BY id DESC",
    )
    .bind(experiment_id)
    .fetch_all(pool)
    .await
}

/// All active hypotheses for an experiment.
pub async fn list_experiment_hypotheses(
    pool: &PgPool,
    experiment_id: i64,
) -> Result<Vec<ExperimentHypothesisRow>, sqlx::Error> {
    let rows: Vec<HypothesisRowTuple> = sqlx::query_as(
        "SELECT id, experiment_id, statement, primary_metric, unit,
                predicted_direction::text, acceptance_criterion::text,
                criterion_locked_at, planned_n, verdict::text
         FROM experiment_hypotheses
         WHERE experiment_id = $1 AND valid_to IS NULL ORDER BY id",
    )
    .bind(experiment_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| ExperimentHypothesisRow {
            id: r.0,
            experiment_id: r.1,
            statement: r.2,
            primary_metric: r.3,
            unit: r.4,
            predicted_direction: r.5,
            acceptance_criterion_json: r.6,
            criterion_locked_at: r.7,
            planned_n: r.8,
            verdict: r.9,
        })
        .collect())
}

/// A row for `experiment_list`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExperimentListRow {
    pub id: i64,
    pub slug: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub project: Option<String>,
    pub updated_at: DateTime<Utc>,
}

/// Paged experiment summaries, filterable by project/kind/status, newest first.
pub async fn list_experiments(
    pool: &PgPool,
    project_id: Option<i32>,
    kind: Option<&str>,
    status: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<ExperimentListRow>, sqlx::Error> {
    sqlx::query_as::<_, ExperimentListRow>(
        "SELECT e.id, e.slug, e.title, e.kind::text AS kind, e.status::text AS status,
                p.name AS project, e.updated_at
         FROM experiments e
         LEFT JOIN projects p ON p.id = e.project_id
         WHERE e.valid_to IS NULL
           AND ($1::int IS NULL OR e.project_id = $1)
           AND ($2::text IS NULL OR e.kind::text = $2)
           AND ($3::text IS NULL OR e.status::text = $3)
         ORDER BY e.updated_at DESC
         LIMIT $4 OFFSET $5",
    )
    .bind(project_id)
    .bind(kind)
    .bind(status)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

/// An ordered event in an experiment's life (for `experiment_timeline`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExperimentEvent {
    pub at: DateTime<Utc>,
    pub event: String,
    pub detail: String,
}

/// The ordered event stream: open → criterion locks → measurement windows →
/// decisions. Built as a UNION over the subsystem tables' timestamps.
pub async fn experiment_timeline(
    pool: &PgPool,
    experiment_id: i64,
) -> Result<Vec<ExperimentEvent>, sqlx::Error> {
    sqlx::query_as::<_, ExperimentEvent>(
        "SELECT created_at AS at, 'opened' AS event, title AS detail
           FROM experiments WHERE id = $1
         UNION ALL
         SELECT criterion_locked_at, 'criterion_locked', statement
           FROM experiment_hypotheses WHERE experiment_id = $1 AND valid_to IS NULL
         UNION ALL
         SELECT r.created_at, 'run', r.arm_label || ' (' || r.arm_kind::text || ')'
           FROM experiment_runs r WHERE r.experiment_id = $1
         UNION ALL
         SELECT created_at, 'decided',
                verdict::text || ' on ' || metric_name || ' (' || test_type || ')'
           FROM experiment_results WHERE experiment_id = $1
         ORDER BY at",
    )
    .bind(experiment_id)
    .fetch_all(pool)
    .await
}

// ============================================================================
// experiment_log_artifact
// ============================================================================

/// Insert an ad-hoc profiling/benchmark/debug artifact. `experiment_id` is
/// `None` for free-standing captures (the "I profiled this, remember it" path).
#[allow(clippy::too_many_arguments)]
pub async fn insert_experiment_artifact(
    pool: &PgPool,
    experiment_id: Option<i64>,
    project_id: Option<i32>,
    kind: &str,
    tool: Option<&str>,
    label: Option<&str>,
    content: Option<&str>,
    content_sha256: Option<&str>,
    metrics_json: &str,
    file_id: Option<i64>,
    embedding: Option<Vector>,
    git_ref: Option<&str>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO experiment_artifacts
            (experiment_id, project_id, kind, tool, label, content, content_sha256,
             metrics, file_id, embedding, git_ref)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8::jsonb, $9, $10, $11)
         RETURNING id",
    )
    .bind(experiment_id)
    .bind(project_id)
    .bind(kind)
    .bind(tool)
    .bind(label)
    .bind(content)
    .bind(content_sha256)
    .bind(metrics_json)
    .bind(file_id)
    .bind(embedding)
    .bind(git_ref)
    .fetch_one(pool)
    .await
}

// ============================================================================
// Web UI read models (the Experiments pane — `src/api/experiments.rs`)
//
// Read-only projections consumed by the daemon's REST surface. `list_experiments`
// (above) filters by numeric `project_id`; the web UI filters by human-readable
// project NAME, so `list_experiments_webui` joins `projects` and matches
// `p.name` — an unknown name yields an EMPTY list rather than the unfiltered set
// (which is what resolving name→id then passing `None` would wrongly produce).
// It also rolls up the first hypothesis statement and the latest decision so a
// list row is self-describing without an N+1 fan-out. `list_experiment_artifacts`
// is the artifact reader the pane's detail view needs (only `insert_*` existed).
// ============================================================================

/// A row for the web UI Experiments list — richer than [`ExperimentListRow`]:
/// it carries the headline hypothesis plus the latest-decision rollup so the
/// list is self-describing. `verdict`/`p_value`/`decided_at` are `NULL` while an
/// experiment is still open (no decision recorded).
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct ExperimentWebuiListRow {
    pub id: i64,
    pub slug: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub project: Option<String>,
    /// First (lowest-id) active hypothesis statement, if any.
    pub hypothesis: Option<String>,
    /// Latest decision's verdict, else the first hypothesis's standing verdict.
    pub verdict: Option<String>,
    /// p-value of the latest decision, if one has been recorded.
    pub p_value: Option<f64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Timestamp of the latest decision (`NULL` while the experiment is open).
    pub decided_at: Option<DateTime<Utc>>,
}

/// Web UI Experiments list: active experiments filtered by project NAME and/or
/// status, newest-updated first. `project_name = None` ⇒ all projects; a name
/// matching no project yields an empty result (not the unfiltered set). `status`
/// is compared against `experiments.status::text` (the closed vocabulary).
pub async fn list_experiments_webui(
    pool: &PgPool,
    project_name: Option<&str>,
    status: Option<&str>,
    limit: i64,
) -> Result<Vec<ExperimentWebuiListRow>, sqlx::Error> {
    sqlx::query_as::<_, ExperimentWebuiListRow>(
        "SELECT e.id, e.slug, e.title, e.kind::text AS kind, e.status::text AS status,
                p.name AS project,
                (SELECT h.statement FROM experiment_hypotheses h
                  WHERE h.experiment_id = e.id AND h.valid_to IS NULL
                  ORDER BY h.id LIMIT 1) AS hypothesis,
                COALESCE(
                  (SELECT r.verdict::text FROM experiment_results r
                    WHERE r.experiment_id = e.id ORDER BY r.id DESC LIMIT 1),
                  (SELECT h.verdict::text FROM experiment_hypotheses h
                    WHERE h.experiment_id = e.id AND h.valid_to IS NULL
                    ORDER BY h.id LIMIT 1)
                ) AS verdict,
                (SELECT r.p_value FROM experiment_results r
                  WHERE r.experiment_id = e.id ORDER BY r.id DESC LIMIT 1) AS p_value,
                e.created_at, e.updated_at,
                (SELECT r.created_at FROM experiment_results r
                  WHERE r.experiment_id = e.id ORDER BY r.id DESC LIMIT 1) AS decided_at
         FROM experiments e
         LEFT JOIN projects p ON p.id = e.project_id
         WHERE e.valid_to IS NULL
           AND ($1::text IS NULL OR p.name = $1)
           AND ($2::text IS NULL OR e.status::text = $2)
         ORDER BY e.updated_at DESC
         LIMIT $3",
    )
    .bind(project_name)
    .bind(status)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// An `experiment_artifacts` row for the web UI. The embedding vector and the
/// FTS-only columns are omitted; `metrics` is the artifact's JSONB blob.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct ExperimentArtifactRow {
    pub id: i64,
    pub kind: String,
    pub tool: Option<String>,
    pub label: Option<String>,
    pub content: Option<String>,
    pub content_sha256: Option<String>,
    pub metrics: serde_json::Value,
    pub git_ref: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// All artifacts attached to a specific experiment, newest first. Free-standing
/// captures (`experiment_id IS NULL`) are intentionally excluded — this backs
/// the per-experiment detail view.
pub async fn list_experiment_artifacts(
    pool: &PgPool,
    experiment_id: i64,
) -> Result<Vec<ExperimentArtifactRow>, sqlx::Error> {
    sqlx::query_as::<_, ExperimentArtifactRow>(
        "SELECT id, kind, tool, label, content, content_sha256, metrics, git_ref, created_at
         FROM experiment_artifacts
         WHERE experiment_id = $1
         ORDER BY created_at DESC, id DESC",
    )
    .bind(experiment_id)
    .fetch_all(pool)
    .await
}
