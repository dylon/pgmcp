//! `ontology-invariants` cron — deterministic invariant & rationale mining
//! (Code-Digital-Twin rationale enrichment, Phase 3).
//!
//! Mines `facet='invariant'` concepts + `constraint_text`/`rationale`/evidence
//! from three already-indexed sources, uniformly via `indexed_files`/`git_commits`
//! (so each carries a `file_id`/`commit_id` for evidence + anchoring):
//!
//! 1. **ADRs** — `docs/decisions/*.md`: one invariant per ADR (the first cued
//!    line; the heading seeds the rationale).
//! 2. **Mandate files** — `CLAUDE.md`/`AGENTS.md`: one invariant per cued line.
//! 3. **Commits** — `git_commits` whose subject/body carries an invariant cue.
//!
//! The same rule from several sources collapses onto **one** concept (merge key
//! = [`crate::ontology::mine::normalize_invariant_name`]) carrying one evidence
//! row per source. Fully **deterministic** — equational canonicalization is a
//! documented future egglog enhancement (see [`crate::ontology::reason`]);
//! LLM-emergent *concept* extraction already lives in the
//! `memory-concept-extract` cron. Idempotent: concept upsert is curation-safe,
//! evidence is `provenance_key`-deduped, and anchoring is existence-guarded — so
//! re-runs add nothing new (and never churn `memory_code_anchor`).

use std::sync::Arc;

use sqlx::PgPool;
use tracing::{error, info};

use crate::config::OntologyConfig;
use crate::db::queries;
use crate::ontology::edge::EvidenceKind;
use crate::ontology::mine::{self, InvariantCandidate};

/// Mine invariants from ADRs, mandate files, and commits; persist concepts +
/// metadata + evidence; refresh the unified-graph matviews.
pub async fn run_ontology_invariants(
    pool: &PgPool,
    config: &OntologyConfig,
) -> Result<(), sqlx::Error> {
    let max = config.max_items_per_run.max(1);
    let mut invariants = 0u64;
    let mut evidence = 0u64;

    // 1. ADRs.
    let adrs: Vec<(i64, String, Option<String>)> = sqlx::query_as(
        "SELECT id, relative_path, content FROM indexed_files
         WHERE content IS NOT NULL
           AND (relative_path LIKE 'docs/decisions/%' OR relative_path LIKE '%/docs/decisions/%')
         ORDER BY id
         LIMIT $1",
    )
    .bind(max)
    .fetch_all(pool)
    .await?;
    for (file_id, path, content) in &adrs {
        let Some(content) = content else { continue };
        if let Some(cand) = mine::extract_adr_invariant(content, path) {
            persist(
                pool,
                &cand,
                EvidenceKind::Adr,
                Some(*file_id),
                None,
                Some(path),
                &mut invariants,
                &mut evidence,
            )
            .await?;
        }
    }

    // 2. Mandate files (CLAUDE.md / AGENTS.md).
    let mandates: Vec<(i64, String, Option<String>)> = sqlx::query_as(
        "SELECT id, relative_path, content FROM indexed_files
         WHERE content IS NOT NULL
           AND (relative_path = 'CLAUDE.md' OR relative_path = 'AGENTS.md'
                OR relative_path LIKE '%/CLAUDE.md' OR relative_path LIKE '%/AGENTS.md')
         ORDER BY id
         LIMIT $1",
    )
    .bind(max)
    .fetch_all(pool)
    .await?;
    for (file_id, path, content) in &mandates {
        let Some(content) = content else { continue };
        for cand in mine::extract_line_invariants(content, "project mandate") {
            persist(
                pool,
                &cand,
                EvidenceKind::Mandate,
                Some(*file_id),
                None,
                Some(path),
                &mut invariants,
                &mut evidence,
            )
            .await?;
        }
    }

    // 3. Commits carrying an invariant cue (pre-filtered in SQL, confirmed in Rust).
    let commits: Vec<(i64, String, Option<String>)> = sqlx::query_as(
        "SELECT id, subject, body FROM git_commits
         WHERE subject ILIKE '%must%' OR subject ILIKE '%never%' OR subject ILIKE '%always%'
            OR subject ILIKE '%invariant%' OR subject ILIKE '%do not%'
            OR body ILIKE '%must %' OR body ILIKE '%invariant%' OR body ILIKE '%never %'
         ORDER BY author_date DESC
         LIMIT $1",
    )
    .bind(max)
    .fetch_all(pool)
    .await?;
    for (commit_id, subject, body) in &commits {
        if let Some(cand) = mine::extract_commit_invariant(subject, body.as_deref()) {
            persist(
                pool,
                &cand,
                EvidenceKind::Commit,
                None,
                Some(*commit_id),
                None,
                &mut invariants,
                &mut evidence,
            )
            .await?;
        }
    }

    // Surface the new invariant concepts/edges in the heterogeneous graph.
    queries::refresh_memory_unified_nodes(pool).await?;
    queries::refresh_memory_unified_edges(pool).await?;
    info!(
        invariant_concepts = invariants,
        evidence_rows = evidence,
        adrs = adrs.len(),
        mandates = mandates.len(),
        commits = commits.len(),
        "ontology-invariants mining complete"
    );
    Ok(())
}

/// Upsert one mined invariant: concept (by merge name) → invariant metadata →
/// idempotent evidence row → existence-guarded source-file anchor.
#[allow(clippy::too_many_arguments)]
async fn persist(
    pool: &PgPool,
    cand: &InvariantCandidate,
    kind: EvidenceKind,
    file_id: Option<i64>,
    commit_id: Option<i64>,
    mandate_ref: Option<&str>,
    invariants: &mut u64,
    evidence: &mut u64,
) -> Result<(), sqlx::Error> {
    let (entity_id, created) =
        queries::memory_upsert_auto_entity(pool, &cand.name, "concept").await?;
    if created {
        *invariants += 1;
    }
    queries::upsert_invariant_meta(
        pool,
        entity_id,
        &cand.constraint_text,
        &cand.rationale,
        "mined",
        None,
    )
    .await?;

    let source_ref = file_id
        .map(|f| format!("f{f}"))
        .or_else(|| commit_id.map(|c| format!("c{c}")))
        .unwrap_or_default();
    let provenance_key = format!("{}:{}:{}", kind.as_str(), source_ref, cand.name);
    if queries::insert_concept_evidence(
        pool,
        entity_id,
        kind,
        commit_id,
        file_id,
        mandate_ref,
        Some(&cand.constraint_text),
        &provenance_key,
    )
    .await?
    {
        *evidence += 1;
    }

    // Anchor the invariant to its source file, idempotently (so the digest can
    // surface it for that file and re-runs never duplicate the anchor).
    if let Some(fid) = file_id {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM memory_code_anchor
             WHERE entity_id = $1 AND file_id = $2 AND anchor_type = 'concept_code')",
        )
        .bind(entity_id)
        .bind(fid)
        .fetch_one(pool)
        .await?;
        if !exists
            && let Err(e) = queries::memory_anchor_entity(
                pool,
                entity_id,
                Some(fid),
                None,
                None,
                None,
                None,
                "concept_code",
            )
            .await
        {
            error!(error = %e, entity_id, "invariant file anchor failed");
        }
    }
    Ok(())
}

/// Cron entry point: run the mining pass, logging (not panicking) on error.
pub async fn run_or_log(pool: Arc<PgPool>, config: OntologyConfig) {
    if let Err(e) = run_ontology_invariants(&pool, &config).await {
        error!(error = %e, "ontology-invariants pass failed");
    }
}
