//! `memory-concept-extract` cron job (Stage 4: auto-population concept layer).
//!
//! Materializes a *sparse, emergent* concept layer in the knowledge graph
//! (`entity_type='concept'`, `source='auto_index'`) — the GraphRAG cross-cutting
//! nodes a pure dependency graph cannot express — in two passes:
//!
//! 1. **Topic-seeded (deterministic, no LLM):** every sufficiently-populated
//!    `code_topics` row becomes a `concept` entity anchored to its topic
//!    (`anchor_type='concept_topic'`), so the concept is graph-connected to the
//!    chunks/files the topic covers. Idempotent: only newly-created concepts are
//!    anchored, so re-runs only seed *new* topics.
//! 2. **LLM-emergent (opt-in):** when `[memory.concepts] llm_enabled` and the
//!    `[memory.extractor]` backend is present, the topic labels are fed to
//!    `LlmExtractor::extract` to surface higher-order concepts + relations
//!    between them. Gated on a content-hash of the grounding text so unchanged
//!    input is skipped (no wasted LLM spend).
//!
//! Never clobbers user/agent/LLM-authored entities (see
//! `queries::memory_upsert_auto_entity`). Refreshes the unified-graph matviews
//! at the end so new concepts appear immediately. Scheduled from
//! `src/cli/daemon.rs`; non-blocking on the cron poll thread.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{error, info};

use crate::config::MemoryConceptsConfig;
use crate::db::queries::{self, NewRelationInput};
use crate::llm::{EntityRef, ExtractionRequest, LlmExtractor, ScopeRef};
use crate::stats::tracker::StatsTracker;

/// Run both concept passes, then refresh the unified-graph matviews.
pub async fn run_memory_concepts(
    pool: &PgPool,
    stats: &StatsTracker,
    config: &MemoryConceptsConfig,
    extractor: Option<&Arc<dyn LlmExtractor>>,
) -> Result<(), sqlx::Error> {
    // (1) Topic-seeded concepts (deterministic).
    let topics = queries::concept_seed_topics(
        pool,
        config.min_chunks_per_topic,
        config.max_concepts_per_run,
    )
    .await?;
    let mut emitted = 0u64;
    // Phase 2: one observation per concept (batched) so the embedding-migration
    // cron embeds it → the concept is vector-searchable via the `observation`
    // matview arm. Deduped by content-sha, so re-runs add nothing new.
    let mut concept_obs: Vec<queries::AddObservationInput> = Vec::with_capacity(topics.len());
    for (topic_id, label) in &topics {
        let (entity_id, created) =
            queries::memory_upsert_auto_entity(pool, label, "concept").await?;
        if created {
            // Anchor the new concept to the topic it summarizes (concept → topic).
            match queries::memory_anchor_entity(
                pool,
                entity_id,
                None,
                None,
                Some(*topic_id),
                None,
                None,
                "concept_topic",
            )
            .await
            {
                Ok(_) => emitted += 1,
                Err(e) => error!(error = %e, topic_id = *topic_id, "concept_topic anchor failed"),
            }
        }

        // Phase 1: classify the concept's facet (deterministic-first) and record
        // the ontology sidecar. Idempotent + curation-safe (see
        // queries::upsert_concept_meta — a curator-set status is never clobbered).
        let facet = crate::ontology::classify::classify_topic_concept(pool, *topic_id, label).await;
        if let Err(e) =
            queries::upsert_concept_meta(pool, entity_id, facet, "topic_seed", None).await
        {
            error!(error = %e, entity_id, "ontology concept meta upsert failed");
        }
        concept_obs.push(queries::AddObservationInput {
            entity_name: label.clone(),
            contents: vec![format!("{label} — code concept")],
        });
    }
    if !concept_obs.is_empty()
        && let Err(e) = queries::memory_add_observations(pool, &concept_obs, "auto_index").await
    {
        error!(error = %e, "concept observation batch insert failed");
    }
    stats
        .memory_concepts_emitted
        .fetch_add(emitted, Ordering::Relaxed);

    // (2) LLM-emergent concepts (opt-in; only when enabled + extractor present).
    if config.llm_enabled
        && let Some(extractor) = extractor
        && let Err(e) = run_llm_concepts(pool, stats, config, extractor.as_ref(), &topics).await
    {
        error!(error = %e, "LLM concept extraction failed");
        stats.memory_concept_errors.fetch_add(1, Ordering::Relaxed);
    }

    stats.memory_concept_runs.fetch_add(1, Ordering::Relaxed);

    // New concepts/edges must surface in the heterogeneous graph immediately.
    queries::refresh_memory_unified_nodes(pool).await?;
    queries::refresh_memory_unified_edges(pool).await?;
    info!(
        topic_concepts = emitted,
        topics = topics.len(),
        "memory-concept-extract pass complete"
    );
    Ok(())
}

/// LLM-emergent pass: feed the topic labels to `LlmExtractor::extract`, persist
/// the emitted concept entities + their relations (`source='auto_index'`).
/// Content-hash-gated so unchanged grounding is skipped.
async fn run_llm_concepts(
    pool: &PgPool,
    stats: &StatsTracker,
    config: &MemoryConceptsConfig,
    extractor: &dyn LlmExtractor,
    topics: &[(i64, String)],
) -> anyhow::Result<()> {
    let grounding: String = topics
        .iter()
        .map(|(_, l)| l.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if grounding.trim().is_empty() {
        return Ok(());
    }
    // Skip re-extraction when the grounding text is unchanged since last run.
    let sig = format!("{:016x}", xxhash_rust::xxh3::xxh3_64(grounding.as_bytes()));
    let prev: Option<String> =
        sqlx::query_scalar("SELECT value FROM pgmcp_metadata WHERE key = 'concept_extract_sig'")
            .fetch_optional(pool)
            .await?;
    if prev.as_deref() == Some(sig.as_str()) {
        stats
            .memory_concept_llm_skips
            .fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }

    // Existing concepts as grounding so the LLM canonicalizes (avoids
    // auth/authentication splits) rather than re-minting near-duplicates.
    let existing: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, name FROM memory_entities
         WHERE entity_type = 'concept' AND valid_to IS NULL
         ORDER BY id DESC LIMIT 100",
    )
    .fetch_all(pool)
    .await?;
    let existing_refs: Vec<EntityRef> = existing
        .into_iter()
        .map(|(id, name)| EntityRef {
            id,
            name,
            entity_type: "concept".to_string(),
            key_observations: Vec::new(),
        })
        .collect();

    let scope = ScopeRef::default();
    let req = ExtractionRequest {
        text: &grounding,
        existing_entities: &existing_refs,
        scope: &scope,
        max_extractions: config.max_concepts_per_run.max(1) as usize,
    };
    // candle inference is sync; keep the runtime responsive.
    let result = tokio::task::block_in_place(|| extractor.extract(req))
        .map_err(|e| anyhow::anyhow!("extractor.extract failed: {e}"))?;

    let mut emitted = 0u64;
    for ent in &result.entities {
        let name = ent.name.trim();
        if name.is_empty() {
            continue;
        }
        let (_id, created) = queries::memory_upsert_auto_entity(pool, name, "concept").await?;
        if created {
            emitted += 1;
        }
    }
    let rels: Vec<NewRelationInput> = result
        .relations
        .iter()
        .map(|r| NewRelationInput {
            from: r.from_name.clone(),
            to: r.to_name.clone(),
            relation_type: r.relation_type.clone(),
        })
        .collect();
    let rel_result = queries::memory_create_relations_detailed(pool, &rels, "auto_index").await?;
    let rel_emitted = rel_result.relations_inserted as u64;

    stats
        .memory_concepts_emitted
        .fetch_add(emitted, Ordering::Relaxed);
    stats
        .memory_concept_relations
        .fetch_add(rel_emitted, Ordering::Relaxed);

    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ('concept_extract_sig', $1)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(&sig)
    .execute(pool)
    .await?;
    info!(
        llm_concepts = emitted,
        llm_relations = rel_emitted,
        "LLM concept extraction complete"
    );
    Ok(())
}

/// Run the concept-extract pass, logging any error rather than panicking the
/// cron thread.
pub async fn run_or_log(
    pool: Arc<PgPool>,
    stats: Arc<StatsTracker>,
    config: MemoryConceptsConfig,
    extractor: Option<Arc<dyn LlmExtractor>>,
) {
    if let Err(e) = run_memory_concepts(&pool, &stats, &config, extractor.as_ref()).await {
        error!(error = %e, "memory-concept-extract pass failed");
    }
}
