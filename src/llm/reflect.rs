//! Memory-server Phase 5: reflection logic — both the agent-driven
//! `memory_reflect` MCP tool body and the `memory-reflect` cron use this
//! module to assemble the observation window, invoke the
//! `LlmExtractor::reflect` call, and persist the higher-order
//! observations.
//!
//! Provenance: emitted observations carry `source = 'reflection'` and
//! `derived_from = [obs_id, ...]` pointing back at the windowed inputs.
//! The same row is `source_session_id`-stamped when an agent triggers
//! reflection from inside a known session.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Result, anyhow};
use sqlx::PgPool;
use tracing::{debug, info, warn};

use crate::db::queries::{self, NewEntityInput};
use crate::llm::LlmExtractor;
use crate::stats::tracker::StatsTracker;

#[derive(Debug, Clone, Copy)]
pub enum ReflectionTrigger {
    Agent,
    Cron,
}

impl ReflectionTrigger {
    fn label(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Cron => "cron",
        }
    }
}

pub struct ReflectionRequest {
    pub scope_id: Option<i64>,
    pub session_id: Option<uuid::Uuid>,
    /// Only consider observations created at or after this time.
    /// `None` = no lower bound.
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    /// Cap on observations included as grounding context. Default 200.
    pub max_observations: i64,
    pub trigger: ReflectionTrigger,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ReflectionReport {
    pub run_id: Option<i64>,
    pub scope_id: Option<i64>,
    pub observations_considered: i64,
    pub entities_emitted: i64,
    pub observations_written: i64,
    pub trigger: &'static str,
}

/// Pull the windowed observation set, invoke the extractor's reflect
/// path, persist the results with full provenance, and log a run row.
pub async fn run_reflection(
    pool: &PgPool,
    stats: &StatsTracker,
    extractor: &dyn LlmExtractor,
    request: ReflectionRequest,
) -> Result<ReflectionReport> {
    let scope_for_query = request.scope_id;
    let max = request.max_observations.clamp(10, 1000);
    let since = request.since;

    // Insert the reflection-runs row early so we can stamp the
    // finished_at + counts even on partial completion.
    let run_id: i64 = sqlx::query_scalar(
        "INSERT INTO memory_reflection_runs
            (scope_id, trigger)
         VALUES ($1, $2)
         RETURNING id",
    )
    .bind(scope_for_query)
    .bind(request.trigger.label())
    .fetch_one(pool)
    .await?;

    // Gather observations within the window. We tag them with their id
    // so `derived_from` can link back.
    let obs_rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT o.id, o.content
         FROM memory_observations o
         JOIN memory_entities e ON e.id = o.entity_id AND e.valid_to IS NULL
         LEFT JOIN memory_entity_scope es ON es.entity_id = e.id
         WHERE o.valid_to IS NULL
           AND ($1::bigint IS NULL OR es.scope_id = $1)
           AND ($2::timestamptz IS NULL OR o.created_at >= $2)
         ORDER BY o.importance DESC, o.created_at DESC
         LIMIT $3",
    )
    .bind(scope_for_query)
    .bind(since)
    .bind(max)
    .fetch_all(pool)
    .await?;

    let observations_considered = obs_rows.len() as i64;
    if observations_considered == 0 {
        finalize_run(pool, run_id, 0, 0).await?;
        match request.trigger {
            ReflectionTrigger::Agent => stats
                .memory_reflection_runs_agent
                .fetch_add(1, Ordering::Relaxed),
            ReflectionTrigger::Cron => stats
                .memory_reflection_runs_cron
                .fetch_add(1, Ordering::Relaxed),
        };
        return Ok(ReflectionReport {
            run_id: Some(run_id),
            scope_id: scope_for_query,
            observations_considered,
            entities_emitted: 0,
            observations_written: 0,
            trigger: request.trigger.label(),
        });
    }

    let obs_ids: Vec<i64> = obs_rows.iter().map(|(id, _)| *id).collect();
    let obs_contents: Vec<String> = obs_rows.iter().map(|(_, c)| c.clone()).collect();

    let reflected = {
        let observations_for_blocking = obs_contents.clone();
        tokio::task::block_in_place(|| extractor.reflect(&observations_for_blocking))
    };
    let reflected = match reflected {
        Ok(r) => r,
        Err(e) => {
            stats
                .memory_reflection_errors
                .fetch_add(1, Ordering::Relaxed);
            warn!(error = %e, run_id, "reflection: LLM call failed");
            finalize_run(pool, run_id, observations_considered, 0).await?;
            return Err(anyhow!("reflection failed: {}", e));
        }
    };

    // Resolve the scope under which to attach reflection-emitted
    // entities. If the caller passed a scope_id we re-use it; otherwise
    // fall back to a NULL-everything (workspace-wide) row.
    let attach_scope_id = match scope_for_query {
        Some(id) => id,
        None => queries::find_or_create_scope(pool, &queries::ScopeSpec::default()).await?,
    };

    // Insert the reflected entities with provenance.
    let entity_inputs: Vec<NewEntityInput> = reflected
        .iter()
        .map(|e| NewEntityInput {
            name: e.name.clone(),
            entity_type: e.entity_type.clone(),
            observations: e.initial_observations.clone(),
        })
        .collect();
    let mut entities_emitted = 0_i64;
    let mut observations_written = 0_i64;
    if !entity_inputs.is_empty() {
        let ids =
            queries::memory_create_entities(pool, &entity_inputs, attach_scope_id, "reflection")
                .await?;
        entities_emitted = ids.len() as i64;
        // Stamp `derived_from` on the just-inserted reflection observations.
        for id in &ids {
            let upd = sqlx::query(
                "UPDATE memory_observations
                    SET derived_from = $1, source_session_id = $2
                  WHERE entity_id = $3 AND source = 'reflection' AND derived_from IS NULL",
            )
            .bind(&obs_ids[..])
            .bind(request.session_id)
            .bind(id)
            .execute(pool)
            .await?;
            observations_written += upd.rows_affected() as i64;
        }
    }

    stats
        .memory_reflection_facts_emitted
        .fetch_add(entities_emitted as u64, Ordering::Relaxed);
    match request.trigger {
        ReflectionTrigger::Agent => stats
            .memory_reflection_runs_agent
            .fetch_add(1, Ordering::Relaxed),
        ReflectionTrigger::Cron => stats
            .memory_reflection_runs_cron
            .fetch_add(1, Ordering::Relaxed),
    };

    finalize_run(pool, run_id, observations_considered, entities_emitted).await?;
    debug!(
        run_id,
        scope_id = ?scope_for_query,
        considered = observations_considered,
        emitted = entities_emitted,
        written = observations_written,
        trigger = request.trigger.label(),
        "reflection: completed",
    );

    Ok(ReflectionReport {
        run_id: Some(run_id),
        scope_id: scope_for_query,
        observations_considered,
        entities_emitted,
        observations_written,
        trigger: request.trigger.label(),
    })
}

async fn finalize_run(
    pool: &PgPool,
    run_id: i64,
    observation_count: i64,
    facts_emitted: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE memory_reflection_runs
            SET finished_at = NOW(),
                observation_count = $1,
                facts_emitted = $2
          WHERE id = $3",
    )
    .bind(observation_count as i32)
    .bind(facts_emitted as i32)
    .bind(run_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Helper used by the cron job: find scopes that crossed the
/// `min_new_observations` threshold since their last completed
/// reflection.
pub async fn scopes_due_for_reflection(pool: &PgPool, min_new: i64) -> Result<Vec<i64>> {
    let rows: Vec<(i64,)> = sqlx::query_as(
        "WITH last_run AS (
             SELECT scope_id, MAX(finished_at) AS finished_at
             FROM memory_reflection_runs
             WHERE scope_id IS NOT NULL
             GROUP BY scope_id
         )
         SELECT es.scope_id
         FROM memory_entity_scope es
         JOIN memory_observations o ON o.entity_id = es.entity_id
         LEFT JOIN last_run lr ON lr.scope_id = es.scope_id
         WHERE o.valid_to IS NULL
           AND (lr.finished_at IS NULL OR o.created_at >= lr.finished_at)
         GROUP BY es.scope_id
         HAVING COUNT(o.id) >= $1
         ORDER BY es.scope_id",
    )
    .bind(min_new)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Cron entry point. Iterates scopes with enough new observations and
/// reflects on each. Caller (the scheduler) is responsible for
/// rate-limiting cadence.
pub async fn run_reflection_cron(
    pool: Arc<PgPool>,
    stats: Arc<StatsTracker>,
    extractor: Arc<dyn LlmExtractor>,
    min_new: i64,
    max_observations: i64,
) {
    let scopes = match scopes_due_for_reflection(&pool, min_new).await {
        Ok(s) => s,
        Err(e) => {
            stats
                .memory_reflection_errors
                .fetch_add(1, Ordering::Relaxed);
            warn!(error = %e, "reflection cron: scope query failed");
            return;
        }
    };
    if scopes.is_empty() {
        debug!("reflection cron: no scopes due");
        return;
    }
    info!(
        scopes = scopes.len(),
        min_new, "reflection cron: scheduling per-scope reflection"
    );
    for scope_id in scopes {
        let request = ReflectionRequest {
            scope_id: Some(scope_id),
            session_id: None,
            since: None,
            max_observations,
            trigger: ReflectionTrigger::Cron,
        };
        if let Err(e) = run_reflection(&pool, &stats, extractor.as_ref(), request).await {
            warn!(error = %e, scope_id, "reflection cron: per-scope failure");
        }
    }
}
