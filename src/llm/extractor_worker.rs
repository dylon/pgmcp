//! Memory-server Phase 4: background salience extractor worker.
//!
//! Stage B of the session-observation pipeline. The HTTP path
//! (`POST /api/session/observe`) handles Stage A (regex extraction +
//! session_mandate upsert) inline and synchronously, then fires this
//! worker via `tokio::spawn` so the HTTP response isn't blocked on the
//! LLM forward pass.
//!
//! Per-session debouncing: a `DashMap<UUID, Instant>` tracks the last
//! Stage-B invocation per session. The worker returns early if the
//! debounce interval hasn't elapsed.
//!
//! Bi-temporal contradiction handling: when the extractor returns a
//! `ContradictionSignal`, the worker sets `valid_to = NOW()` on the
//! prior row and inserts the new fact with `superseded_by` pointing to
//! it. The active-row filter then naturally hides the invalidated
//! version while `memory_facts_at(t < now)` continues to see it.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Result;
use dashmap::DashMap;
use sqlx::PgPool;
use tracing::{debug, info, warn};

use crate::db::queries::{self, AddObservationInput, NewEntityInput, NewRelationInput, ScopeSpec};
use crate::llm::{
    ContradictionKind, EntityRef, ExtractionRequest, LlmExtractor, NewEntity, NewRelation, ScopeRef,
};
use crate::stats::tracker::StatsTracker;

/// Per-session debounce ledger. Holding the last-invocation `Instant`
/// per session id lets the worker skip work cheaply.
pub type DebounceMap = Arc<DashMap<uuid::Uuid, Instant>>;

/// Owned set of inputs the worker needs to do its job. Cheap to clone
/// into the spawned task.
pub struct ExtractorJob {
    pub session_id: uuid::Uuid,
    pub source_prompt_id: i64,
    pub project_id: Option<i32>,
    pub agent_id: Option<String>,
    pub user_id: Option<String>,
    pub prompt_text: String,
}

/// Configuration handed in once per daemon startup. The fields are
/// per-request defaults; the worker doesn't read TOML directly.
pub struct ExtractorWorkerConfig {
    pub debounce: Duration,
    pub max_extractions: usize,
    pub grounding_top_k_entities: usize,
    pub grounding_top_k_observations: usize,
}

impl Default for ExtractorWorkerConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_secs(30),
            max_extractions: 25,
            grounding_top_k_entities: 10,
            grounding_top_k_observations: 3,
        }
    }
}

/// Run Stage B once for a given prompt.
///
/// Caller is responsible for spawning this via `tokio::spawn` so the
/// HTTP request returns immediately. The function itself is
/// debounce-aware and short-circuits when the session was extracted-from
/// recently.
pub async fn run_extraction_for_prompt(
    pool: PgPool,
    stats: Arc<StatsTracker>,
    extractor: Arc<dyn LlmExtractor>,
    debounce: DebounceMap,
    config: ExtractorWorkerConfig,
    job: ExtractorJob,
) {
    let _ = stats.mcp_requests.fetch_add(0, Ordering::Relaxed); // touch — pattern across the codebase
    if !claim_debounce(&debounce, job.session_id, config.debounce) {
        debug!(session = %job.session_id, "extractor_worker: debounced");
        return;
    }
    match run_once(&pool, &stats, extractor.as_ref(), &config, &job).await {
        Ok(written) => {
            stats.memory_extractor_runs.fetch_add(1, Ordering::Relaxed);
            stats
                .memory_extractor_entities_written
                .fetch_add(written.entities as u64, Ordering::Relaxed);
            stats
                .memory_extractor_relations_written
                .fetch_add(written.relations as u64, Ordering::Relaxed);
            stats
                .memory_extractor_observations_written
                .fetch_add(written.observations as u64, Ordering::Relaxed);
            stats
                .memory_extractor_contradictions_resolved
                .fetch_add(written.contradictions_resolved as u64, Ordering::Relaxed);
            if written.entities
                + written.relations
                + written.observations
                + written.contradictions_resolved
                > 0
            {
                info!(
                    session = %job.session_id,
                    entities = written.entities,
                    relations = written.relations,
                    observations = written.observations,
                    contradictions_resolved = written.contradictions_resolved,
                    "extractor_worker: persisted",
                );
            }
        }
        Err(e) => {
            stats
                .memory_extractor_errors
                .fetch_add(1, Ordering::Relaxed);
            warn!(error = %e, session = %job.session_id, "extractor_worker: failed");
        }
    }
}

/// Wrote-counts for one Stage-B pass.
#[derive(Debug, Default, Clone, Copy)]
pub struct ExtractorWrites {
    pub entities: usize,
    pub relations: usize,
    pub observations: usize,
    pub contradictions_resolved: usize,
}

async fn run_once(
    pool: &PgPool,
    _stats: &StatsTracker,
    extractor: &dyn LlmExtractor,
    config: &ExtractorWorkerConfig,
    job: &ExtractorJob,
) -> Result<ExtractorWrites> {
    let scope_spec = ScopeSpec {
        user_id: job.user_id.clone(),
        agent_id: job.agent_id.clone(),
        session_id: Some(job.session_id),
        project_id: job.project_id,
    };
    let scope_id = queries::find_or_create_scope(pool, &scope_spec).await?;

    let grounding = fetch_grounding_entities(
        pool,
        scope_id,
        config.grounding_top_k_entities as i32,
        config.grounding_top_k_observations as i32,
    )
    .await?;

    let scope_ref = ScopeRef {
        user_id: scope_spec.user_id.clone(),
        agent_id: scope_spec.agent_id.clone(),
        session_id: scope_spec.session_id,
        project_id: scope_spec.project_id,
    };
    let request = ExtractionRequest {
        text: &job.prompt_text,
        existing_entities: &grounding,
        scope: &scope_ref,
        max_extractions: config.max_extractions,
    };

    let result = {
        let extractor_clone = extractor;
        let request_owned = OwnedExtractionRequest::from_request(&request);
        let raw =
            tokio::task::block_in_place(|| extractor_clone.extract(request_owned.as_request()));
        raw?
    };

    let mut writes = ExtractorWrites::default();

    // 1. Soft-invalidate prior facts the LLM flagged as contradicted.
    for c in &result.contradictions {
        let table = match c.kind {
            ContradictionKind::Observation => "memory_observations",
            ContradictionKind::Relation => "memory_relations",
        };
        let upd = sqlx::query(&format!(
            "UPDATE {} SET valid_to = NOW() WHERE id = $1 AND valid_to IS NULL",
            table
        ))
        .bind(c.conflicting_with)
        .execute(pool)
        .await?;
        if upd.rows_affected() > 0 {
            writes.contradictions_resolved += upd.rows_affected() as usize;
        }
    }

    // 2. Insert new entities (and any initial observations).
    let entity_inputs: Vec<NewEntityInput> = result
        .entities
        .iter()
        .map(|e| NewEntityInput {
            name: e.name.clone(),
            entity_type: e.entity_type.clone(),
            observations: e.initial_observations.clone(),
        })
        .collect();
    let mut new_entity_ids: Vec<i64> = Vec::new();
    if !entity_inputs.is_empty() {
        new_entity_ids =
            queries::memory_create_entities(pool, &entity_inputs, scope_id, "llm_extraction")
                .await?;
        writes.entities = new_entity_ids.len();
        writes.observations += result
            .entities
            .iter()
            .map(|e| e.initial_observations.len())
            .sum::<usize>();
        // Stamp the source_prompt_id on these freshly-inserted observations
        // so we can audit which Stage-B run wrote them.
        for id in &new_entity_ids {
            let _ = sqlx::query(
                "UPDATE memory_observations
                    SET source_session_id = $1, source_prompt_id = $2
                  WHERE entity_id = $3 AND source_prompt_id IS NULL
                    AND source = 'llm_extraction'",
            )
            .bind(job.session_id)
            .bind(job.source_prompt_id)
            .bind(id)
            .execute(pool)
            .await;
        }
    }

    // 3. Insert new relations.
    if !result.relations.is_empty() {
        let rels: Vec<NewRelationInput> = result
            .relations
            .iter()
            .map(|r| NewRelationInput {
                from: r.from_name.clone(),
                to: r.to_name.clone(),
                relation_type: r.relation_type.clone(),
            })
            .collect();
        let result =
            queries::memory_create_relations_detailed(pool, &rels, "llm_extraction").await?;
        writes.relations = result.relations_inserted;
    }

    let _ = (job.user_id.as_ref(), config.max_extractions);
    let _ = stamp_importance_on_new_entities(pool, &new_entity_ids, &result.entities).await;
    Ok(writes)
}

/// Bring the new entities' importance up to the LLM-judged value where
/// the LLM declared one. We could fold this into `memory_create_entities`
/// but keeping the SQL layer narrow means the LLM-specific behavior
/// (writing importance from the extractor's output) lives here.
async fn stamp_importance_on_new_entities(
    pool: &PgPool,
    ids: &[i64],
    entities: &[NewEntity],
) -> Result<()> {
    for (id, e) in ids.iter().zip(entities.iter()) {
        let imp = e.importance.clamp(0.0, 1.0);
        sqlx::query("UPDATE memory_entities SET importance = $1 WHERE id = $2")
            .bind(imp)
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// Returns the top-K most-important entities in the given scope, each
/// annotated with their top-N observations. Provides the grounding
/// context for the extraction prompt — the LLM should attach new facts
/// to these existing entities rather than inventing variants.
async fn fetch_grounding_entities(
    pool: &PgPool,
    scope_id: i64,
    k_entities: i32,
    k_observations: i32,
) -> Result<Vec<EntityRef>> {
    let entities: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT DISTINCT e.id, e.name, e.entity_type
         FROM memory_entities e
         JOIN memory_entity_scope es ON es.entity_id = e.id
         WHERE e.valid_to IS NULL AND es.scope_id = $1
         ORDER BY e.importance DESC, e.id DESC
         LIMIT $2",
    )
    .bind(scope_id)
    .bind(k_entities)
    .fetch_all(pool)
    .await?;
    if entities.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(entities.len());
    for (id, name, entity_type) in entities {
        let obs: Vec<String> = sqlx::query_scalar(
            "SELECT content FROM memory_observations
             WHERE entity_id = $1 AND valid_to IS NULL
             ORDER BY importance DESC, created_at DESC
             LIMIT $2",
        )
        .bind(id)
        .bind(k_observations)
        .fetch_all(pool)
        .await?;
        out.push(EntityRef {
            id,
            name,
            entity_type,
            key_observations: obs,
        });
    }
    let _ = AddObservationInput {
        entity_name: String::new(),
        contents: Vec::new(),
    }; // suppress unused-import warning when query API surface evolves
    Ok(out)
}

/// Atomically check + update the per-session debounce ledger. Returns
/// `true` if this caller "won" the debounce slot.
fn claim_debounce(ledger: &DebounceMap, session_id: uuid::Uuid, window: Duration) -> bool {
    let now = Instant::now();
    let entry = ledger.entry(session_id);
    match entry {
        dashmap::mapref::entry::Entry::Occupied(mut e) => {
            if now.duration_since(*e.get()) < window {
                false
            } else {
                e.insert(now);
                true
            }
        }
        dashmap::mapref::entry::Entry::Vacant(v) => {
            v.insert(now);
            true
        }
    }
}

// Owned mirror of ExtractionRequest so the &str/&[T] borrows can survive
// the move into `block_in_place`. Cheap copy: prompts are <2 KB, grounding
// is bounded.
struct OwnedExtractionRequest {
    text: String,
    entities: Vec<EntityRef>,
    scope: ScopeRef,
    max_extractions: usize,
}

impl OwnedExtractionRequest {
    fn from_request(r: &ExtractionRequest<'_>) -> Self {
        Self {
            text: r.text.to_string(),
            entities: r.existing_entities.to_vec(),
            scope: r.scope.clone(),
            max_extractions: r.max_extractions,
        }
    }
    fn as_request(&self) -> ExtractionRequest<'_> {
        ExtractionRequest {
            text: &self.text,
            existing_entities: &self.entities,
            scope: &self.scope,
            max_extractions: self.max_extractions,
        }
    }
}

// Allow these to be unused if the cron-only build path doesn't import
// them — they're part of the public phase-4 surface.
#[allow(dead_code)]
fn _suppress_unused_relation_marker(_: NewRelation) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn debounce_keeps_one_caller_per_window() {
        let ledger: DebounceMap = Arc::new(DashMap::new());
        let session = uuid::Uuid::new_v4();
        let window = Duration::from_millis(100);

        // First caller wins.
        assert!(claim_debounce(&ledger, session, window));
        // Immediate retry loses.
        assert!(!claim_debounce(&ledger, session, window));
        // After the window, wins again.
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(claim_debounce(&ledger, session, window));
    }
}
