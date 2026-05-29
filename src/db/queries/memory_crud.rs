//! Memory-graph CRUD + lifecycle (scopes/entities/relations/observations,
//! forget/retention, invariant-eval reporting). Extracted from `queries.rs` (god-file split).
#![allow(unused_imports)]

use crate::db::queries::*;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

// ============================================================================
// Memory-server Phase 2 + 3: knowledge-graph CRUD queries
// ============================================================================
//
// Drop-in replacement surface for `@modelcontextprotocol/server-memory` —
// entities + relations + observations stored in PostgreSQL with
// bi-temporal columns. See `docs/memory-server/05-schema.md` for the
// schema and `docs/memory-server/06-tools.md` for the tool catalog.

/// Scope tuple. Each dimension is optional; NULL means "any". Used both
/// as a search filter (find entities visible under this scope) and as an
/// attachment key (create_entities attaches to this scope row).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ScopeSpec {
    pub user_id: Option<String>,
    pub agent_id: Option<String>,
    pub session_id: Option<uuid::Uuid>,
    pub project_id: Option<i32>,
}

/// Find an existing `memory_scope` row matching the spec, or create one.
/// Returns the scope id.
///
/// Postgres 15+ supports `UNIQUE NULLS NOT DISTINCT`; on older versions
/// we fall back to an `INSERT ... WHERE NOT EXISTS` race-tolerant path.
pub async fn find_or_create_scope(pool: &PgPool, scope: &ScopeSpec) -> Result<i64, sqlx::Error> {
    if let Some(id) = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM memory_scope
         WHERE user_id IS NOT DISTINCT FROM $1
           AND agent_id IS NOT DISTINCT FROM $2
           AND session_id IS NOT DISTINCT FROM $3
           AND project_id IS NOT DISTINCT FROM $4
         LIMIT 1",
    )
    .bind(scope.user_id.as_deref())
    .bind(scope.agent_id.as_deref())
    .bind(scope.session_id)
    .bind(scope.project_id)
    .fetch_optional(pool)
    .await?
    {
        return Ok(id);
    }

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO memory_scope (user_id, agent_id, session_id, project_id)
         VALUES ($1, $2, $3, $4)
         RETURNING id",
    )
    .bind(scope.user_id.as_deref())
    .bind(scope.agent_id.as_deref())
    .bind(scope.session_id)
    .bind(scope.project_id)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Compute a sha256 hex digest. Mirrors `sessions::prompt_sha256` but
/// kept local to this module to avoid the API surface widening.
fn observation_sha256(content: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    format!("{:x}", h.finalize())
}

/// `memory_create_entities` payload row.
#[derive(Debug, Clone)]
pub struct NewEntityInput {
    pub name: String,
    pub entity_type: String,
    /// Initial observations attached at entity-creation time. May be empty.
    pub observations: Vec<String>,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct EntityRow {
    pub id: i64,
    pub name: String,
    pub entity_type: String,
    pub canonical_name: Option<String>,
    pub importance: f32,
    pub source: String,
    pub created_at: DateTime<Utc>,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
    pub superseded_by: Option<i64>,
}

/// Create entities (and optionally initial observations) under the given
/// scope. Returns the inserted entity ids in input order. Idempotent on
/// `(name, entity_type)` when an active row exists — re-using the prior
/// id and appending observations.
/// Stage-4 auto-population: upsert an auto-derived `concept` entity. Reuses any
/// active row with the same (name, entity_type) WITHOUT modifying it — so a
/// user/agent/LLM-authored entity is never clobbered (the caller can still link
/// to the returned id) — and inserts a fresh `source='auto_index'` entity only
/// when none exists. Returns `(entity_id, created_new)`.
pub async fn memory_upsert_auto_entity(
    pool: &PgPool,
    name: &str,
    entity_type: &str,
) -> Result<(i64, bool), sqlx::Error> {
    if let Some(id) = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM memory_entities
         WHERE name = $1 AND entity_type = $2 AND valid_to IS NULL
         LIMIT 1",
    )
    .bind(name)
    .bind(entity_type)
    .fetch_optional(pool)
    .await?
    {
        return Ok((id, false));
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, importance, source)
         VALUES ($1, $2, 0.5, 'auto_index'::memory_source)
         RETURNING id",
    )
    .bind(name)
    .bind(entity_type)
    .fetch_one(pool)
    .await?;
    Ok((id, true))
}

/// Stage-4: candidate topics for concept seeding — labeled topics with at least
/// `min_chunks` member chunks, most-populous first.
pub async fn concept_seed_topics(
    pool: &PgPool,
    min_chunks: i64,
    limit: i64,
) -> Result<Vec<(i64, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, label FROM code_topics
         WHERE chunk_count >= $1 AND label IS NOT NULL AND btrim(label) <> ''
         ORDER BY chunk_count DESC
         LIMIT $2",
    )
    .bind(min_chunks)
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn memory_create_entities(
    pool: &PgPool,
    inputs: &[NewEntityInput],
    scope_id: i64,
    source: &str,
) -> Result<Vec<i64>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut out = Vec::with_capacity(inputs.len());

    for input in inputs {
        // Re-use the active row if one exists; otherwise insert.
        let existing: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM memory_entities
             WHERE name = $1 AND entity_type = $2 AND valid_to IS NULL
             LIMIT 1",
        )
        .bind(&input.name)
        .bind(&input.entity_type)
        .fetch_optional(&mut *tx)
        .await?;

        let entity_id: i64 = match existing {
            Some(id) => id,
            None => {
                sqlx::query_scalar(
                    "INSERT INTO memory_entities
                        (name, entity_type, importance, source)
                     VALUES ($1, $2, 0.5, $3::memory_source)
                     RETURNING id",
                )
                .bind(&input.name)
                .bind(&input.entity_type)
                .bind(source)
                .fetch_one(&mut *tx)
                .await?
            }
        };

        // Attach scope (idempotent).
        sqlx::query(
            "INSERT INTO memory_entity_scope (entity_id, scope_id)
             VALUES ($1, $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(entity_id)
        .bind(scope_id)
        .execute(&mut *tx)
        .await?;

        // Append observations (idempotent on (entity_id, content_sha256, valid_from);
        // re-creating the same observation gets eaten by the UNIQUE).
        for obs in &input.observations {
            let sha = observation_sha256(obs);
            let _ = sqlx::query(
                "INSERT INTO memory_observations
                    (entity_id, content, content_sha256, source)
                 VALUES ($1, $2, $3, $4::memory_source)
                 ON CONFLICT DO NOTHING",
            )
            .bind(entity_id)
            .bind(obs)
            .bind(&sha)
            .bind(source)
            .execute(&mut *tx)
            .await?;
        }

        out.push(entity_id);
    }

    tx.commit().await?;
    Ok(out)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NewRelationInput {
    pub from: String,
    pub to: String,
    pub relation_type: String,
}

// Renamed from `RelationRow` during the 2026-05-29 god-file split: the
// pre-existing `work_items` submodule also defines a `RelationRow`, and once
// both were exposed behind `pub use ...::*` globs the duplicate name tripped
// `ambiguous_glob_reexports` (a clippy `-D warnings` failure). Memory readers
// return `RelationDump`, not this row, so it has no callers and the
// disambiguating rename is behavior-preserving.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct MemoryRelationRow {
    pub id: i64,
    pub from_entity_id: i64,
    pub to_entity_id: i64,
    pub relation_type: String,
    pub importance: f32,
    pub source: String,
    pub created_at: DateTime<Utc>,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
}

/// Create relations between existing entities (looked up by name). Returns
/// the inserted relation ids; -1 sentinel for entries whose endpoints
/// couldn't be found.
pub async fn memory_create_relations(
    pool: &PgPool,
    inputs: &[NewRelationInput],
    source: &str,
) -> Result<Vec<i64>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut out = Vec::with_capacity(inputs.len());

    for input in inputs {
        // Resolve endpoints (active rows only).
        let from_id: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM memory_entities WHERE name = $1 AND valid_to IS NULL LIMIT 1",
        )
        .bind(&input.from)
        .fetch_optional(&mut *tx)
        .await?;
        let to_id: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM memory_entities WHERE name = $1 AND valid_to IS NULL LIMIT 1",
        )
        .bind(&input.to)
        .fetch_optional(&mut *tx)
        .await?;
        let (Some(from_id), Some(to_id)) = (from_id, to_id) else {
            out.push(-1);
            continue;
        };
        if from_id == to_id {
            out.push(-1);
            continue;
        }

        // Existing active relation with same triple? Reuse.
        let existing: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM memory_relations
             WHERE from_entity_id = $1 AND to_entity_id = $2 AND relation_type = $3
               AND valid_to IS NULL
             LIMIT 1",
        )
        .bind(from_id)
        .bind(to_id)
        .bind(&input.relation_type)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(id) = existing {
            out.push(id);
            continue;
        }

        let id: i64 = sqlx::query_scalar(
            "INSERT INTO memory_relations
                (from_entity_id, to_entity_id, relation_type, source)
             VALUES ($1, $2, $3, $4::memory_source)
             RETURNING id",
        )
        .bind(from_id)
        .bind(to_id)
        .bind(&input.relation_type)
        .bind(source)
        .fetch_one(&mut *tx)
        .await?;
        out.push(id);
    }

    tx.commit().await?;
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct AddObservationInput {
    pub entity_name: String,
    pub contents: Vec<String>,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ObservationRow {
    pub id: i64,
    pub entity_id: i64,
    pub content: String,
    pub importance: f32,
    pub source: String,
    pub created_at: DateTime<Utc>,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
}

/// Append observations to an existing entity. Returns ids of newly-inserted
/// observations (skips duplicates via the UNIQUE constraint). The caller
/// can detect missing entities by counting fewer returned ids than inputs.
pub async fn memory_add_observations(
    pool: &PgPool,
    inputs: &[AddObservationInput],
    source: &str,
) -> Result<Vec<i64>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut out = Vec::new();

    for input in inputs {
        let entity_id: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM memory_entities WHERE name = $1 AND valid_to IS NULL LIMIT 1",
        )
        .bind(&input.entity_name)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(entity_id) = entity_id else {
            continue;
        };

        for content in &input.contents {
            let sha = observation_sha256(content);
            let inserted: Option<i64> = sqlx::query_scalar(
                "INSERT INTO memory_observations
                    (entity_id, content, content_sha256, source)
                 VALUES ($1, $2, $3, $4::memory_source)
                 ON CONFLICT DO NOTHING
                 RETURNING id",
            )
            .bind(entity_id)
            .bind(content)
            .bind(&sha)
            .bind(source)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(id) = inserted {
                out.push(id);
            }
        }
    }

    tx.commit().await?;
    Ok(out)
}

/// Soft-delete entities by name. Sets `valid_to = NOW()` on the active
/// row for each name; observations and relations remain queryable via
/// `memory_facts_at(t < deletion_time)` per the bi-temporal contract.
///
/// Returns the number of entity rows affected.
pub async fn memory_delete_entities(pool: &PgPool, names: &[String]) -> Result<u64, sqlx::Error> {
    if names.is_empty() {
        return Ok(0);
    }
    let res = sqlx::query(
        "UPDATE memory_entities
            SET valid_to = NOW()
          WHERE name = ANY($1) AND valid_to IS NULL",
    )
    .bind(names)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

#[derive(Debug, Clone)]
pub struct DeleteObservationInput {
    pub entity_name: String,
    pub observations: Vec<String>,
}

/// Soft-delete observations by content text under a named entity.
pub async fn memory_delete_observations(
    pool: &PgPool,
    inputs: &[DeleteObservationInput],
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut affected = 0_u64;
    for input in inputs {
        let entity_id: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM memory_entities WHERE name = $1 AND valid_to IS NULL LIMIT 1",
        )
        .bind(&input.entity_name)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(entity_id) = entity_id else {
            continue;
        };
        for content in &input.observations {
            let res = sqlx::query(
                "UPDATE memory_observations
                    SET valid_to = NOW()
                  WHERE entity_id = $1 AND content = $2 AND valid_to IS NULL",
            )
            .bind(entity_id)
            .bind(content)
            .execute(&mut *tx)
            .await?;
            affected += res.rows_affected();
        }
    }
    tx.commit().await?;
    Ok(affected)
}

/// Soft-delete relations matching `(from_name, to_name, relation_type)`.
pub async fn memory_delete_relations(
    pool: &PgPool,
    inputs: &[NewRelationInput],
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut affected = 0_u64;
    for input in inputs {
        let res = sqlx::query(
            "UPDATE memory_relations r
                SET valid_to = NOW()
              FROM memory_entities a, memory_entities b
              WHERE r.from_entity_id = a.id
                AND r.to_entity_id = b.id
                AND a.name = $1 AND a.valid_to IS NULL
                AND b.name = $2 AND b.valid_to IS NULL
                AND r.relation_type = $3
                AND r.valid_to IS NULL",
        )
        .bind(&input.from)
        .bind(&input.to)
        .bind(&input.relation_type)
        .execute(&mut *tx)
        .await?;
        affected += res.rows_affected();
    }
    tx.commit().await?;
    Ok(affected)
}

// ============================================================================
// Memory-server Phase 8: forget + retention queries
// ============================================================================

/// What kind of memory row to forget. Used by `memory_forget` and the
/// audit log.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgetTargetType {
    Entity,
    Observation,
    Relation,
}

impl ForgetTargetType {
    pub fn label(self) -> &'static str {
        match self {
            Self::Entity => "entity",
            Self::Observation => "observation",
            Self::Relation => "relation",
        }
    }
    pub fn parse(s: &str) -> Result<Self, sqlx::Error> {
        match s {
            "entity" => Ok(Self::Entity),
            "observation" => Ok(Self::Observation),
            "relation" => Ok(Self::Relation),
            other => Err(sqlx::Error::Protocol(format!(
                "unknown target_type '{}'; expected entity|observation|relation",
                other
            ))),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ForgetReport {
    pub target_type: String,
    pub target_id: i64,
    pub cascade: bool,
    pub rows_affected: i64,
    pub manifest: serde_json::Value,
    pub forget_log_id: i64,
}

/// Phase 8.4: forget an entity / observation / relation. `cascade=false`
/// (default) sets `valid_to = NOW()` (soft delete); `cascade=true`
/// physically deletes the row + dependent rows and writes the manifest
/// to `memory_forget_log`.
pub async fn memory_forget(
    pool: &PgPool,
    target_type: ForgetTargetType,
    target_id: i64,
    cascade: bool,
    actor: &str,
) -> Result<ForgetReport, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let rows_affected: i64;
    let mut manifest = serde_json::json!({});

    match target_type {
        ForgetTargetType::Entity => {
            if cascade {
                let (obs_count,): (i64,) =
                    sqlx::query_as("SELECT COUNT(*) FROM memory_observations WHERE entity_id = $1")
                        .bind(target_id)
                        .fetch_one(&mut *tx)
                        .await?;
                let (rel_count,): (i64,) = sqlx::query_as(
                    "SELECT COUNT(*) FROM memory_relations
                     WHERE from_entity_id = $1 OR to_entity_id = $1",
                )
                .bind(target_id)
                .fetch_one(&mut *tx)
                .await?;
                let (anchor_count,): (i64,) =
                    sqlx::query_as("SELECT COUNT(*) FROM memory_code_anchor WHERE entity_id = $1")
                        .bind(target_id)
                        .fetch_one(&mut *tx)
                        .await?;
                let (scope_count,): (i64,) =
                    sqlx::query_as("SELECT COUNT(*) FROM memory_entity_scope WHERE entity_id = $1")
                        .bind(target_id)
                        .fetch_one(&mut *tx)
                        .await?;
                let (tier_count,): (i64,) =
                    sqlx::query_as("SELECT COUNT(*) FROM memory_entity_tier WHERE entity_id = $1")
                        .bind(target_id)
                        .fetch_one(&mut *tx)
                        .await?;
                manifest = serde_json::json!({
                    "observations": obs_count,
                    "relations": rel_count,
                    "code_anchors": anchor_count,
                    "scopes": scope_count,
                    "tiers": tier_count,
                });
                let res = sqlx::query("DELETE FROM memory_entities WHERE id = $1")
                    .bind(target_id)
                    .execute(&mut *tx)
                    .await?;
                rows_affected = res.rows_affected() as i64
                    + obs_count
                    + rel_count
                    + anchor_count
                    + scope_count
                    + tier_count;
            } else {
                let res = sqlx::query(
                    "UPDATE memory_entities SET valid_to = NOW()
                     WHERE id = $1 AND valid_to IS NULL",
                )
                .bind(target_id)
                .execute(&mut *tx)
                .await?;
                rows_affected = res.rows_affected() as i64;
            }
        }
        ForgetTargetType::Observation => {
            if cascade {
                let res = sqlx::query("DELETE FROM memory_observations WHERE id = $1")
                    .bind(target_id)
                    .execute(&mut *tx)
                    .await?;
                rows_affected = res.rows_affected() as i64;
            } else {
                let res = sqlx::query(
                    "UPDATE memory_observations SET valid_to = NOW()
                     WHERE id = $1 AND valid_to IS NULL",
                )
                .bind(target_id)
                .execute(&mut *tx)
                .await?;
                rows_affected = res.rows_affected() as i64;
            }
        }
        ForgetTargetType::Relation => {
            if cascade {
                let res = sqlx::query("DELETE FROM memory_relations WHERE id = $1")
                    .bind(target_id)
                    .execute(&mut *tx)
                    .await?;
                rows_affected = res.rows_affected() as i64;
            } else {
                let res = sqlx::query(
                    "UPDATE memory_relations SET valid_to = NOW()
                     WHERE id = $1 AND valid_to IS NULL",
                )
                .bind(target_id)
                .execute(&mut *tx)
                .await?;
                rows_affected = res.rows_affected() as i64;
            }
        }
    };

    let forget_log_id: i64 = sqlx::query_scalar(
        "INSERT INTO memory_forget_log
            (actor, target_type, target_id, cascade, rows_affected, manifest_json)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id",
    )
    .bind(actor)
    .bind(target_type.label())
    .bind(target_id)
    .bind(cascade)
    .bind(rows_affected as i32)
    .bind(&manifest)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(ForgetReport {
        target_type: target_type.label().to_string(),
        target_id,
        cascade,
        rows_affected,
        manifest,
        forget_log_id,
    })
}

/// Phase 8.2 dry-run for the retention cron. Returns counts of rows
/// that *would* be hard-deleted given the window + importance
/// threshold, without touching any rows.
pub async fn memory_retention_dry_run(
    pool: &PgPool,
    window_days: i64,
    importance_threshold: f32,
) -> Result<(i64, i64, i64), sqlx::Error> {
    let (e,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM memory_entities
         WHERE valid_to IS NOT NULL
           AND valid_to < NOW() - ($1::int * interval '1 day')
           AND importance < $2",
    )
    .bind(window_days as i32)
    .bind(importance_threshold)
    .fetch_one(pool)
    .await?;
    let (o,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM memory_observations
         WHERE valid_to IS NOT NULL
           AND valid_to < NOW() - ($1::int * interval '1 day')
           AND importance < $2",
    )
    .bind(window_days as i32)
    .bind(importance_threshold)
    .fetch_one(pool)
    .await?;
    let (r,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM memory_relations
         WHERE valid_to IS NOT NULL
           AND valid_to < NOW() - ($1::int * interval '1 day')
           AND importance < $2",
    )
    .bind(window_days as i32)
    .bind(importance_threshold)
    .fetch_one(pool)
    .await?;
    Ok((e, o, r))
}

/// Phase 8.2: hard-delete soft-deleted rows past the retention window
/// AND below the importance threshold AND not pointed at by any
/// `superseded_by` chain. Returns (entities, observations, relations)
/// deleted.
pub async fn memory_retention_purge(
    pool: &PgPool,
    window_days: i64,
    importance_threshold: f32,
) -> Result<(u64, u64, u64), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let e = sqlx::query(
        "DELETE FROM memory_entities
         WHERE valid_to IS NOT NULL
           AND valid_to < NOW() - ($1::int * interval '1 day')
           AND importance < $2
           AND id NOT IN (
               SELECT superseded_by FROM memory_entities
               WHERE superseded_by IS NOT NULL
           )
           -- G2: never purge promoted best practices (procedural/reflective tier).
           AND id NOT IN (
               SELECT entity_id FROM memory_entity_tier
               WHERE tier IN ('procedural','reflective')
           )",
    )
    .bind(window_days as i32)
    .bind(importance_threshold)
    .execute(&mut *tx)
    .await?;
    let o = sqlx::query(
        "DELETE FROM memory_observations
         WHERE valid_to IS NOT NULL
           AND valid_to < NOW() - ($1::int * interval '1 day')
           AND importance < $2
           AND id NOT IN (
               SELECT superseded_by FROM memory_observations
               WHERE superseded_by IS NOT NULL
           )
           -- G2: never purge best-practice observations or outcome-linked rows.
           AND entity_id NOT IN (
               SELECT entity_id FROM memory_entity_tier
               WHERE tier IN ('procedural','reflective')
           )
           AND id NOT IN (
               SELECT observation_id FROM agent_outcomes
               WHERE observation_id IS NOT NULL
           )",
    )
    .bind(window_days as i32)
    .bind(importance_threshold)
    .execute(&mut *tx)
    .await?;
    let r = sqlx::query(
        "DELETE FROM memory_relations
         WHERE valid_to IS NOT NULL
           AND valid_to < NOW() - ($1::int * interval '1 day')
           AND importance < $2
           AND id NOT IN (
               SELECT superseded_by FROM memory_relations
               WHERE superseded_by IS NOT NULL
           )",
    )
    .bind(window_days as i32)
    .bind(importance_threshold)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((e.rows_affected(), o.rows_affected(), r.rows_affected()))
}

/// Phase 9: memory-server invariant report. Each field is the count of
/// rows that violate the corresponding invariant. A clean memory graph
/// returns zeros across the board.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MemoryEvalReport {
    /// Rows where `valid_to <= valid_from` (impossible by design).
    pub entities_temporal_invalid: i64,
    pub observations_temporal_invalid: i64,
    pub relations_temporal_invalid: i64,
    /// `superseded_by` chains that include a cycle (root reaches itself).
    pub entity_supersede_cycles: i64,
    pub observation_supersede_cycles: i64,
    pub relation_supersede_cycles: i64,
    /// Observations whose `entity_id` does not match any entity row
    /// (would normally be caught by FK; included for defense in depth).
    pub orphan_observations: i64,
    /// `derived_from` arrays in reflective observations that point at
    /// rows that no longer exist — purely an audit metric, not a fault.
    pub reflection_derived_from_missing: i64,
    /// Code-anchors whose target file/chunk/topic no longer exists.
    pub stale_code_anchors: i64,
    /// `memory_forget_log` entries whose claimed `target_id` still
    /// exists in the target table with `valid_to IS NULL` (suggests
    /// the forget didn't actually take effect).
    pub forget_log_dangling: i64,
    pub rows_examined: i64,
}

/// Phase 9: scan the memory tables for bi-temporal / provenance /
/// referential-integrity violations. Bounded by `row_cap` per table —
/// the count fields are exact within that bound, so a daemon with a
/// 50-million-row memory graph still finishes in seconds.
pub async fn memory_eval_invariants(
    pool: &PgPool,
    row_cap: i64,
) -> Result<MemoryEvalReport, sqlx::Error> {
    let mut r = MemoryEvalReport {
        rows_examined: row_cap,
        ..Default::default()
    };

    r.entities_temporal_invalid = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
           SELECT 1 FROM memory_entities
            WHERE valid_to IS NOT NULL AND valid_to <= valid_from
            LIMIT $1
         ) sub",
    )
    .bind(row_cap)
    .fetch_one(pool)
    .await?;
    r.observations_temporal_invalid = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
           SELECT 1 FROM memory_observations
            WHERE valid_to IS NOT NULL AND valid_to <= valid_from
            LIMIT $1
         ) sub",
    )
    .bind(row_cap)
    .fetch_one(pool)
    .await?;
    r.relations_temporal_invalid = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
           SELECT 1 FROM memory_relations
            WHERE valid_to IS NOT NULL AND valid_to <= valid_from
            LIMIT $1
         ) sub",
    )
    .bind(row_cap)
    .fetch_one(pool)
    .await?;

    r.entity_supersede_cycles = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
           WITH RECURSIVE walk AS (
             SELECT id AS root, superseded_by AS next, 1 AS depth
               FROM memory_entities WHERE superseded_by IS NOT NULL
             UNION ALL
             SELECT w.root, e.superseded_by, w.depth + 1
               FROM walk w
               JOIN memory_entities e ON e.id = w.next
              WHERE w.next IS NOT NULL AND w.depth < 32
           )
           SELECT 1 FROM walk WHERE next = root LIMIT $1
         ) sub",
    )
    .bind(row_cap)
    .fetch_one(pool)
    .await?;

    r.observation_supersede_cycles = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
           WITH RECURSIVE walk AS (
             SELECT id AS root, superseded_by AS next, 1 AS depth
               FROM memory_observations WHERE superseded_by IS NOT NULL
             UNION ALL
             SELECT w.root, o.superseded_by, w.depth + 1
               FROM walk w
               JOIN memory_observations o ON o.id = w.next
              WHERE w.next IS NOT NULL AND w.depth < 32
           )
           SELECT 1 FROM walk WHERE next = root LIMIT $1
         ) sub",
    )
    .bind(row_cap)
    .fetch_one(pool)
    .await?;

    r.relation_supersede_cycles = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
           WITH RECURSIVE walk AS (
             SELECT id AS root, superseded_by AS next, 1 AS depth
               FROM memory_relations WHERE superseded_by IS NOT NULL
             UNION ALL
             SELECT w.root, rel.superseded_by, w.depth + 1
               FROM walk w
               JOIN memory_relations rel ON rel.id = w.next
              WHERE w.next IS NOT NULL AND w.depth < 32
           )
           SELECT 1 FROM walk WHERE next = root LIMIT $1
         ) sub",
    )
    .bind(row_cap)
    .fetch_one(pool)
    .await?;

    r.orphan_observations = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
           SELECT 1 FROM memory_observations o
            LEFT JOIN memory_entities e ON e.id = o.entity_id
            WHERE e.id IS NULL
            LIMIT $1
         ) sub",
    )
    .bind(row_cap)
    .fetch_one(pool)
    .await?;

    r.reflection_derived_from_missing = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
           SELECT o.id FROM memory_observations o
            WHERE o.source = 'reflection'
              AND o.derived_from IS NOT NULL
              AND NOT EXISTS (
                SELECT 1 FROM memory_observations src
                 WHERE src.id = ANY(o.derived_from)
              )
            LIMIT $1
         ) sub",
    )
    .bind(row_cap)
    .fetch_one(pool)
    .await?;

    r.stale_code_anchors = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
           SELECT a.id
             FROM memory_code_anchor a
             LEFT JOIN indexed_files f ON f.id = a.file_id
             LEFT JOIN file_chunks   c ON c.id = a.chunk_id
             LEFT JOIN code_topics   t ON t.id = a.topic_id
            WHERE (a.file_id  IS NOT NULL AND f.id IS NULL)
               OR (a.chunk_id IS NOT NULL AND c.id IS NULL)
               OR (a.topic_id IS NOT NULL AND t.id IS NULL)
            LIMIT $1
         ) sub",
    )
    .bind(row_cap)
    .fetch_one(pool)
    .await?;

    r.forget_log_dangling = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
           SELECT fl.id
             FROM memory_forget_log fl
            WHERE fl.cascade = false
              AND (
                   (fl.target_type = 'entity' AND EXISTS (
                       SELECT 1 FROM memory_entities e
                        WHERE e.id = fl.target_id AND e.valid_to IS NULL
                   ))
                OR (fl.target_type = 'observation' AND EXISTS (
                       SELECT 1 FROM memory_observations o
                        WHERE o.id = fl.target_id AND o.valid_to IS NULL
                   ))
                OR (fl.target_type = 'relation' AND EXISTS (
                       SELECT 1 FROM memory_relations rel
                        WHERE rel.id = fl.target_id AND rel.valid_to IS NULL
                   ))
              )
            LIMIT $1
         ) sub",
    )
    .bind(row_cap)
    .fetch_one(pool)
    .await?;

    Ok(r)
}

/// Persist a memory-eval invariant report into `pgmcp_metadata` so
/// daemons can surface "last successful eval" without standing up a
/// separate table. Stored as a single JSON blob keyed by
/// `memory_eval_last_report`.
pub async fn record_memory_eval_report(
    pool: &PgPool,
    report: &MemoryEvalReport,
) -> Result<(), sqlx::Error> {
    let body = serde_json::json!({
        "report": report,
        "recorded_at": chrono::Utc::now(),
    });
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ('memory_eval_last_report', $1)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(body.to_string())
    .execute(pool)
    .await?;
    Ok(())
}
