//! DB read/write for the `csm_*` tables (ADR-009). Pure persistence — all
//! protocol/projection/conformance logic lives in the sibling modules.

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::csm::conformance::{Event, TranscriptTurn};

/// Persist a pattern run's structured transcript onto the parent `a2a_tasks`
/// row's metadata (under `csm_transcript`) so the observer can lift it later.
/// Additive and best-effort: it never changes the run's control flow.
pub async fn record_run_transcript(
    pool: &PgPool,
    parent_task_id: Uuid,
    turns: &[TranscriptTurn],
) -> Result<(), sqlx::Error> {
    let json = serde_json::to_value(turns).unwrap_or_else(|_| Value::Array(Vec::new()));
    sqlx::query(
        "UPDATE a2a_tasks
            SET metadata = COALESCE(metadata, '{}'::jsonb)
                           || jsonb_build_object('csm_transcript', $2::jsonb)
          WHERE id = $1",
    )
    .bind(parent_task_id)
    .bind(json)
    .execute(pool)
    .await?;
    Ok(())
}

/// Convenience: build [`TranscriptTurn`]s from a pattern tool's in-memory
/// transcript (entries `{round, role, output}`) and persist them. The
/// `converged` flag is derived from a Reflector turn whose output carries the
/// `CONVERGED` marker (Deliberation). Other patterns ignore `converged`.
pub async fn record_transcript_values(
    pool: &PgPool,
    parent_task_id: Uuid,
    transcript: &[Value],
) -> Result<(), sqlx::Error> {
    let turns = crate::csm::conformance::transcript_to_turns(transcript);
    record_run_transcript(pool, parent_task_id, &turns).await
}

/// Read a run's `skill_id` and lifted transcript turns (empty if none recorded).
pub async fn read_run(
    pool: &PgPool,
    task_id: Uuid,
) -> Result<Option<(Option<String>, Vec<TranscriptTurn>)>, sqlx::Error> {
    let row: Option<(Option<String>, Value)> = sqlx::query_as(
        "SELECT skill_id, COALESCE(metadata->'csm_transcript', '[]'::jsonb)
           FROM a2a_tasks WHERE id = $1",
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(skill, turns_json)| {
        let turns: Vec<TranscriptTurn> = serde_json::from_value(turns_json).unwrap_or_default();
        (skill, turns)
    }))
}

/// Insert a validated run trace (with its MSM-encoded series and optional link
/// to the RLM `agent_trajectories` row); returns the new row id.
pub async fn insert_run_trace(
    pool: &PgPool,
    task_id: Uuid,
    protocol_name: &str,
    conformant: bool,
    conformance_error: Option<&str>,
    events: &[Event],
    encoded: &[f64],
    trajectory_id: Option<i64>,
) -> Result<i64, sqlx::Error> {
    let events_json = serde_json::to_value(events).unwrap_or_else(|_| Value::Array(Vec::new()));
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO csm_run_traces
            (task_id, protocol_name, conformant, conformance_error, events,
             encoded_series, trajectory_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING id",
    )
    .bind(task_id)
    .bind(protocol_name)
    .bind(conformant)
    .bind(conformance_error)
    .bind(events_json)
    .bind(encoded)
    .bind(trajectory_id)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Load prior validated runs of a protocol as MSM cohorts:
/// `(conformant_rows, non_conformant_rows)`, each `(run_trace_id, encoded_series)`,
/// skipping empty series. Feeds `trajectory_index::classify_trend`.
pub async fn load_protocol_cohorts(
    pool: &PgPool,
    protocol_name: &str,
) -> Result<(Vec<(i64, Vec<f64>)>, Vec<(i64, Vec<f64>)>), sqlx::Error> {
    let rows: Vec<(i64, bool, Vec<f64>)> = sqlx::query_as(
        "SELECT id, conformant, encoded_series
           FROM csm_run_traces
          WHERE protocol_name = $1 AND cardinality(encoded_series) > 0",
    )
    .bind(protocol_name)
    .fetch_all(pool)
    .await?;
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    for (id, conformant, series) in rows {
        if conformant {
            ok.push((id, series));
        } else {
            bad.push((id, series));
        }
    }
    Ok((ok, bad))
}

/// Find the RLM trajectory recorded for a task (the recursive pattern), as
/// `(trajectory_id, total_subcalls, encoded_series)`. Matches on `task_id` or
/// `parent_task_id` (the recursive tool persists under the parent task id).
pub async fn find_trajectory_for_task(
    pool: &PgPool,
    task_id: Uuid,
) -> Result<Option<(i64, i32, Vec<f64>)>, sqlx::Error> {
    let row: Option<(i64, i32, Vec<f64>)> = sqlx::query_as(
        "SELECT id, total_subcalls, encoded_series
           FROM agent_trajectories
          WHERE task_id = $1 OR parent_task_id = $1
          ORDER BY created_at DESC
          LIMIT 1",
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Load the lifted event traces of a protocol's validated runs as symbol
/// sequences (`from->to:label`) for Phase-8 passive FSM inference.
pub async fn load_protocol_event_traces(
    pool: &PgPool,
    protocol_name: &str,
) -> Result<Vec<Vec<String>>, sqlx::Error> {
    let rows: Vec<(Value,)> =
        sqlx::query_as("SELECT events FROM csm_run_traces WHERE protocol_name = $1")
            .bind(protocol_name)
            .fetch_all(pool)
            .await?;
    let mut traces = Vec::with_capacity(rows.len());
    for (events,) in rows {
        if let Value::Array(arr) = events {
            let mut seq = Vec::with_capacity(arr.len());
            for ev in &arr {
                let from = ev.get("from").and_then(|v| v.as_str()).unwrap_or("?");
                let to = ev.get("to").and_then(|v| v.as_str()).unwrap_or("?");
                let label = ev
                    .get("label")
                    .and_then(|l| l.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                seq.push(format!("{from}->{to}:{label}"));
            }
            if !seq.is_empty() {
                traces.push(seq);
            }
        }
    }
    Ok(traces)
}

/// `(total_runs, conformant_runs)` recorded for a protocol.
pub async fn protocol_run_stats(
    pool: &PgPool,
    protocol_name: &str,
) -> Result<(i64, i64), sqlx::Error> {
    let row: (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*)::int8,
                COUNT(*) FILTER (WHERE conformant)::int8
           FROM csm_run_traces WHERE protocol_name = $1",
    )
    .bind(protocol_name)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Upsert a protocol's encoded global type into the registry; returns its id.
pub async fn upsert_protocol(
    pool: &PgPool,
    name: &str,
    skill_id: &str,
    global_json: &Value,
    participants: &[String],
    wellformed: bool,
) -> Result<i64, sqlx::Error> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO csm_protocols
            (name, pattern_skill_id, global_type, participants, wellformed, updated_at)
         VALUES ($1, $2, $3, $4, $5, NOW())
         ON CONFLICT (name) DO UPDATE
            SET pattern_skill_id = EXCLUDED.pattern_skill_id,
                global_type      = EXCLUDED.global_type,
                participants     = EXCLUDED.participants,
                wellformed       = EXCLUDED.wellformed,
                updated_at       = NOW()
         RETURNING id",
    )
    .bind(name)
    .bind(skill_id)
    .bind(global_json)
    .bind(participants)
    .bind(wellformed)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Cache one role's projection (or the error explaining why it does not project).
pub async fn upsert_projection(
    pool: &PgPool,
    protocol_id: i64,
    role: &str,
    local_json: Option<&Value>,
    n_states: i32,
    projection_error: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO csm_projections
            (protocol_id, role, local_type, n_states, projection_error)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (protocol_id, role) DO UPDATE
            SET local_type       = EXCLUDED.local_type,
                n_states         = EXCLUDED.n_states,
                projection_error = EXCLUDED.projection_error",
    )
    .bind(protocol_id)
    .bind(role)
    .bind(local_json)
    .bind(n_states)
    .bind(projection_error)
    .execute(pool)
    .await?;
    Ok(())
}
