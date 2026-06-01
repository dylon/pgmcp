//! Memory-graph readers: node/semantic/hybrid search, temporal facts-at,
//! traversal, unified-node search, neighbors, PPR/PathRAG, RAPTOR, code anchors.
//! Extracted from `queries.rs` (god-file split).
#![allow(unused_imports)]

use crate::db::queries::*;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// Substring/ILIKE search across entity names, types, and observation
/// content (Phase 3 baseline; semantic search is `memory_semantic_search`
/// in §3.2). Scope-filtered when `scope_id` is `Some`.
///
/// Returns the matched entities (deduped) with their observation hit
/// count. Limited to `limit` rows.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct EntitySearchHit {
    pub id: i64,
    pub name: String,
    pub entity_type: String,
    pub canonical_name: Option<String>,
    pub importance: f32,
    pub matched_observations: i64,
}

pub async fn memory_search_nodes(
    pool: &PgPool,
    query: &str,
    scope_id: Option<i64>,
    limit: i32,
) -> Result<Vec<EntitySearchHit>, sqlx::Error> {
    let like = format!("%{}%", query);
    sqlx::query_as::<_, EntitySearchHit>(
        "SELECT e.id, e.name, e.entity_type, e.canonical_name, e.importance,
                COUNT(o.id) FILTER (WHERE o.content ILIKE $1) AS matched_observations
         FROM memory_entities e
         LEFT JOIN memory_observations o
            ON o.entity_id = e.id AND o.valid_to IS NULL
         LEFT JOIN memory_entity_scope es ON es.entity_id = e.id
         WHERE e.valid_to IS NULL
           AND ($2::bigint IS NULL OR es.scope_id = $2)
           AND (
             e.name ILIKE $1
             OR e.entity_type ILIKE $1
             OR e.canonical_name ILIKE $1
             OR o.content ILIKE $1
           )
         GROUP BY e.id
         ORDER BY matched_observations DESC, e.importance DESC, e.id
         LIMIT $3",
    )
    .bind(&like)
    .bind(scope_id)
    .bind(limit.clamp(1, 500))
    .fetch_all(pool)
    .await
}

/// Read entities + their observations + their relations by name (active
/// rows only). The official server's `open_nodes`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OpenedNode {
    pub entity: EntityRow,
    pub observations: Vec<String>,
    pub relations_out: Vec<NewRelationInput>,
    pub relations_in: Vec<NewRelationInput>,
}

pub async fn memory_open_nodes(
    pool: &PgPool,
    names: &[String],
) -> Result<Vec<OpenedNode>, sqlx::Error> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let entities = sqlx::query_as::<_, EntityRow>(
        "SELECT id, name, entity_type, canonical_name, importance,
                source::text AS source, created_at, valid_from, valid_to, superseded_by
         FROM memory_entities
         WHERE name = ANY($1) AND valid_to IS NULL",
    )
    .bind(names)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(entities.len());
    for e in entities {
        let obs: Vec<String> = sqlx::query_scalar(
            "SELECT content FROM memory_observations
             WHERE entity_id = $1 AND valid_to IS NULL
             ORDER BY created_at",
        )
        .bind(e.id)
        .fetch_all(pool)
        .await?;

        let rel_out: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT a.name AS from_name, b.name AS to_name, r.relation_type
             FROM memory_relations r
             JOIN memory_entities a ON a.id = r.from_entity_id
             JOIN memory_entities b ON b.id = r.to_entity_id
             WHERE r.from_entity_id = $1 AND r.valid_to IS NULL",
        )
        .bind(e.id)
        .fetch_all(pool)
        .await?;
        let rel_in: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT a.name AS from_name, b.name AS to_name, r.relation_type
             FROM memory_relations r
             JOIN memory_entities a ON a.id = r.from_entity_id
             JOIN memory_entities b ON b.id = r.to_entity_id
             WHERE r.to_entity_id = $1 AND r.valid_to IS NULL",
        )
        .bind(e.id)
        .fetch_all(pool)
        .await?;
        let relations_out = rel_out
            .into_iter()
            .map(|(from, to, rt)| NewRelationInput {
                from,
                to,
                relation_type: rt,
            })
            .collect();
        let relations_in = rel_in
            .into_iter()
            .map(|(from, to, rt)| NewRelationInput {
                from,
                to,
                relation_type: rt,
            })
            .collect();
        out.push(OpenedNode {
            entity: e,
            observations: obs,
            relations_out,
            relations_in,
        });
    }
    Ok(out)
}

/// Full-graph dump (active rows only) for the given scope or workspace-
/// wide when `scope_id` is `None`. Returns entities, observations, and
/// relations as parallel arrays.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryGraphDump {
    pub entities: Vec<EntityRow>,
    pub observations: Vec<ObservationRow>,
    pub relations: Vec<RelationDump>,
    pub entity_count: i64,
    pub observation_count: i64,
    pub relation_count: i64,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct RelationDump {
    pub id: i64,
    pub from_entity_id: i64,
    pub to_entity_id: i64,
    pub from_name: String,
    pub to_name: String,
    pub relation_type: String,
}

pub async fn memory_read_graph(
    pool: &PgPool,
    scope_id: Option<i64>,
    limit_entities: i32,
) -> Result<MemoryGraphDump, sqlx::Error> {
    let limit = limit_entities.clamp(1, 2000);
    let entities = sqlx::query_as::<_, EntityRow>(
        "SELECT DISTINCT e.id, e.name, e.entity_type, e.canonical_name, e.importance,
                e.source::text AS source, e.created_at, e.valid_from,
                e.valid_to, e.superseded_by
         FROM memory_entities e
         LEFT JOIN memory_entity_scope es ON es.entity_id = e.id
         WHERE e.valid_to IS NULL
           AND ($1::bigint IS NULL OR es.scope_id = $1)
         ORDER BY e.importance DESC, e.id
         LIMIT $2",
    )
    .bind(scope_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let ids: Vec<i64> = entities.iter().map(|e| e.id).collect();
    let observations: Vec<ObservationRow> = if ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            "SELECT id, entity_id, content, importance, source::text AS source,
                    created_at, valid_from, valid_to
             FROM memory_observations
             WHERE entity_id = ANY($1) AND valid_to IS NULL
             ORDER BY entity_id, created_at",
        )
        .bind(&ids)
        .fetch_all(pool)
        .await?
    };

    let relations: Vec<RelationDump> = if ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            "SELECT r.id, r.from_entity_id, r.to_entity_id,
                    a.name AS from_name, b.name AS to_name, r.relation_type
             FROM memory_relations r
             JOIN memory_entities a ON a.id = r.from_entity_id
             JOIN memory_entities b ON b.id = r.to_entity_id
             WHERE r.valid_to IS NULL
               AND (r.from_entity_id = ANY($1) OR r.to_entity_id = ANY($1))",
        )
        .bind(&ids)
        .fetch_all(pool)
        .await?
    };

    let entity_count = entities.len() as i64;
    let observation_count = observations.len() as i64;
    let relation_count = relations.len() as i64;
    Ok(MemoryGraphDump {
        entities,
        observations,
        relations,
        entity_count,
        observation_count,
        relation_count,
    })
}

// ============================================================================
// Memory-server Phase 3.2: pgmcp retrieval extensions
// ============================================================================
//
// Beyond the official-compat substring `memory_search_nodes`, these
// extensions add vector / hybrid / bi-temporal / graph-traversal /
// code-anchor surfaces. See `docs/memory-server/06-tools.md` Phase 3.2.

/// Semantic search over `memory_observations.embedding` (BGE-M3 dense).
/// Returns the top-k observations matching the query embedding, joined
/// with their parent entities and scope-filtered when `scope_id` is
/// `Some`.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct MemorySemanticHit {
    pub observation_id: i64,
    pub entity_id: i64,
    pub entity_name: String,
    pub entity_type: String,
    pub content: String,
    pub importance: f32,
    pub similarity: Option<f64>,
    pub created_at: DateTime<Utc>,
}

pub async fn memory_semantic_search(
    pool: &PgPool,
    embedding: &[f32],
    scope_id: Option<i64>,
    tier: Option<&str>,
    limit: i32,
    ef_search: i32,
) -> Result<Vec<MemorySemanticHit>, sqlx::Error> {
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "memory_semantic_search: expected 1024d embedding, got {}",
            embedding.len()
        )));
    }
    let v = pgvector::Vector::from(embedding.to_vec());
    let mut tx = pool.begin().await?;
    sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", ef_search))
        .execute(&mut *tx)
        .await?;

    let rows = sqlx::query_as::<_, MemorySemanticHit>(
        "SELECT o.id AS observation_id,
                e.id AS entity_id,
                e.name AS entity_name,
                e.entity_type,
                o.content,
                o.importance,
                1 - (o.embedding <=> $1) AS similarity,
                o.created_at
         FROM memory_observations o
         JOIN memory_entities e ON e.id = o.entity_id AND e.valid_to IS NULL
         LEFT JOIN memory_entity_scope es ON es.entity_id = e.id
         LEFT JOIN memory_entity_tier  et ON et.entity_id = e.id
         WHERE o.embedding IS NOT NULL
           AND o.valid_to IS NULL
           AND ($2::bigint IS NULL OR es.scope_id = $2)
           AND ($3::text   IS NULL OR et.tier::text = $3)
         ORDER BY o.embedding <=> $1
         LIMIT $4",
    )
    .bind(&v)
    .bind(scope_id)
    .bind(tier)
    .bind(limit.clamp(1, 200))
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(rows)
}

/// Hybrid memory search: RRF fusion of FTS over observation content +
/// dense vector cosine. Mirrors the existing `hybrid_search` (file
/// chunks) but over `memory_observations`.
pub async fn memory_hybrid_search(
    pool: &PgPool,
    query_text: &str,
    embedding: &[f32],
    scope_id: Option<i64>,
    tier: Option<&str>,
    limit: i32,
    ef_search: i32,
) -> Result<Vec<MemorySemanticHit>, sqlx::Error> {
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "memory_hybrid_search: expected 1024d embedding, got {}",
            embedding.len()
        )));
    }
    let v = pgvector::Vector::from(embedding.to_vec());
    let k = limit.clamp(1, 200);
    let pool_size = (k * 3).clamp(20, 300);
    let mut tx = pool.begin().await?;
    sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", ef_search))
        .execute(&mut *tx)
        .await?;

    let rows = sqlx::query_as::<_, MemorySemanticHit>(
        "WITH dense AS (
            SELECT o.id, o.entity_id, o.content, o.importance, o.created_at,
                   1 - (o.embedding <=> $1) AS sim,
                   ROW_NUMBER() OVER (ORDER BY o.embedding <=> $1) AS rnk
            FROM memory_observations o
            JOIN memory_entities e ON e.id = o.entity_id AND e.valid_to IS NULL
            LEFT JOIN memory_entity_scope es ON es.entity_id = e.id
            LEFT JOIN memory_entity_tier  et ON et.entity_id = e.id
            WHERE o.embedding IS NOT NULL AND o.valid_to IS NULL
              AND ($3::bigint IS NULL OR es.scope_id = $3)
              AND ($4::text   IS NULL OR et.tier::text = $4)
            ORDER BY o.embedding <=> $1
            LIMIT $5
         ),
         sparse AS (
            SELECT o.id, o.entity_id, o.content, o.importance, o.created_at,
                   NULL::float8 AS sim,
                   ROW_NUMBER() OVER (
                       ORDER BY ts_rank_cd(
                          to_tsvector('english', o.content),
                          plainto_tsquery('english', $2)
                       ) DESC
                   ) AS rnk
            FROM memory_observations o
            JOIN memory_entities e ON e.id = o.entity_id AND e.valid_to IS NULL
            LEFT JOIN memory_entity_scope es ON es.entity_id = e.id
            LEFT JOIN memory_entity_tier  et ON et.entity_id = e.id
            WHERE o.valid_to IS NULL
              AND ($3::bigint IS NULL OR es.scope_id = $3)
              AND ($4::text   IS NULL OR et.tier::text = $4)
              AND to_tsvector('english', o.content) @@ plainto_tsquery('english', $2)
            LIMIT $5
         ),
         fused AS (
            SELECT id, entity_id, content, importance, created_at, sim,
                   SUM(1.0 / (60.0 + rnk)) AS rrf
            FROM (
                 SELECT * FROM dense
                 UNION ALL
                 SELECT * FROM sparse
            ) u
            GROUP BY id, entity_id, content, importance, created_at, sim
         )
         SELECT f.id AS observation_id,
                e.id AS entity_id,
                e.name AS entity_name,
                e.entity_type,
                f.content,
                f.importance,
                f.sim AS similarity,
                f.created_at
         FROM fused f
         JOIN memory_entities e ON e.id = f.entity_id
         ORDER BY rrf DESC
         LIMIT $6",
    )
    .bind(&v)
    .bind(query_text)
    .bind(scope_id)
    .bind(tier)
    .bind(pool_size)
    .bind(k)
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(rows)
}

/// Bi-temporal point-in-time snapshot.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryFactsAtSnapshot {
    pub as_of: DateTime<Utc>,
    pub entities: Vec<EntityRow>,
    pub observations: Vec<ObservationRow>,
    pub relations: Vec<RelationDump>,
}

pub async fn memory_facts_at(
    pool: &PgPool,
    as_of: DateTime<Utc>,
    scope_id: Option<i64>,
    tier: Option<&str>,
    limit_entities: i32,
) -> Result<MemoryFactsAtSnapshot, sqlx::Error> {
    let limit = limit_entities.clamp(1, 2000);
    let entities = sqlx::query_as::<_, EntityRow>(
        "SELECT DISTINCT e.id, e.name, e.entity_type, e.canonical_name, e.importance,
                e.source::text AS source, e.created_at, e.valid_from,
                e.valid_to, e.superseded_by
         FROM memory_entities e
         LEFT JOIN memory_entity_scope es ON es.entity_id = e.id
         LEFT JOIN memory_entity_tier  et ON et.entity_id = e.id
         WHERE e.valid_from <= $1
           AND (e.valid_to IS NULL OR e.valid_to > $1)
           AND ($2::bigint IS NULL OR es.scope_id = $2)
           AND ($3::text   IS NULL OR et.tier::text = $3)
         ORDER BY e.importance DESC, e.id
         LIMIT $4",
    )
    .bind(as_of)
    .bind(scope_id)
    .bind(tier)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let ids: Vec<i64> = entities.iter().map(|e| e.id).collect();
    let observations: Vec<ObservationRow> = if ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            "SELECT id, entity_id, content, importance, source::text AS source,
                    created_at, valid_from, valid_to
             FROM memory_observations
             WHERE entity_id = ANY($1)
               AND valid_from <= $2
               AND (valid_to IS NULL OR valid_to > $2)
             ORDER BY entity_id, created_at",
        )
        .bind(&ids)
        .bind(as_of)
        .fetch_all(pool)
        .await?
    };

    let relations: Vec<RelationDump> = if ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            "SELECT r.id, r.from_entity_id, r.to_entity_id,
                    a.name AS from_name, b.name AS to_name, r.relation_type
             FROM memory_relations r
             JOIN memory_entities a ON a.id = r.from_entity_id
             JOIN memory_entities b ON b.id = r.to_entity_id
             WHERE r.valid_from <= $2
               AND (r.valid_to IS NULL OR r.valid_to > $2)
               AND (r.from_entity_id = ANY($1) OR r.to_entity_id = ANY($1))",
        )
        .bind(&ids)
        .bind(as_of)
        .fetch_all(pool)
        .await?
    };

    Ok(MemoryFactsAtSnapshot {
        as_of,
        entities,
        observations,
        relations,
    })
}

/// BFS relation-traversal from one or more seed entities.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryTraversalNode {
    pub entity_id: i64,
    pub name: String,
    pub entity_type: String,
    pub depth: i32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryTraversal {
    pub seeds: Vec<i64>,
    pub nodes: Vec<MemoryTraversalNode>,
    pub edges: Vec<RelationDump>,
}

pub async fn memory_relations_traverse(
    pool: &PgPool,
    seed_ids: &[i64],
    max_depth: i32,
    relation_filter: Option<&str>,
    max_nodes: i32,
) -> Result<MemoryTraversal, sqlx::Error> {
    if seed_ids.is_empty() {
        return Ok(MemoryTraversal {
            seeds: Vec::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
        });
    }
    let depth_cap = max_depth.clamp(1, 6);
    let node_cap = max_nodes.clamp(1, 1000);

    let rows = sqlx::query_as::<_, (i64, String, String, i32)>(
        "WITH RECURSIVE frontier(entity_id, name, entity_type, depth) AS (
             SELECT e.id, e.name, e.entity_type, 0::int
             FROM memory_entities e
             WHERE e.id = ANY($1) AND e.valid_to IS NULL
             UNION
             SELECT e2.id, e2.name, e2.entity_type, f.depth + 1
             FROM frontier f
             JOIN memory_relations r
                  ON  (r.from_entity_id = f.entity_id OR r.to_entity_id = f.entity_id)
                  AND r.valid_to IS NULL
                  AND ($2::text IS NULL OR r.relation_type = $2)
             JOIN memory_entities e2
                  ON e2.id = CASE WHEN r.from_entity_id = f.entity_id
                                  THEN r.to_entity_id
                                  ELSE r.from_entity_id
                              END
                  AND e2.valid_to IS NULL
             WHERE f.depth < $3
         )
         SELECT entity_id, name, entity_type, MIN(depth)::int AS depth
         FROM frontier
         GROUP BY entity_id, name, entity_type
         ORDER BY MIN(depth), entity_id
         LIMIT $4",
    )
    .bind(seed_ids)
    .bind(relation_filter)
    .bind(depth_cap)
    .bind(node_cap)
    .fetch_all(pool)
    .await?;

    let nodes: Vec<MemoryTraversalNode> = rows
        .into_iter()
        .map(|(id, name, entity_type, depth)| MemoryTraversalNode {
            entity_id: id,
            name,
            entity_type,
            depth,
        })
        .collect();
    let node_ids: Vec<i64> = nodes.iter().map(|n| n.entity_id).collect();

    let edges: Vec<RelationDump> = if node_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            "SELECT r.id, r.from_entity_id, r.to_entity_id,
                    a.name AS from_name, b.name AS to_name, r.relation_type
             FROM memory_relations r
             JOIN memory_entities a ON a.id = r.from_entity_id
             JOIN memory_entities b ON b.id = r.to_entity_id
             WHERE r.valid_to IS NULL
               AND r.from_entity_id = ANY($1)
               AND r.to_entity_id = ANY($1)
               AND ($2::text IS NULL OR r.relation_type = $2)",
        )
        .bind(&node_ids)
        .bind(relation_filter)
        .fetch_all(pool)
        .await?
    };

    Ok(MemoryTraversal {
        seeds: seed_ids.to_vec(),
        nodes,
        edges,
    })
}

// ============================================================================
// Memory-server Phase 3.2: code-anchor cross-graph queries
// ============================================================================

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct MemoryCodeAnchorRow {
    pub id: i64,
    pub entity_id: i64,
    pub file_id: Option<i64>,
    pub chunk_id: Option<i64>,
    pub topic_id: Option<i64>,
    pub symbol_id: Option<i64>,
    pub project_id: Option<i32>,
    pub anchor_type: String,
    pub created_at: DateTime<Utc>,
}

#[allow(clippy::too_many_arguments)]
pub async fn memory_anchor_entity(
    pool: &PgPool,
    entity_id: i64,
    file_id: Option<i64>,
    chunk_id: Option<i64>,
    topic_id: Option<i64>,
    symbol_id: Option<i64>,
    project_id: Option<i32>,
    anchor_type: &str,
) -> Result<i64, sqlx::Error> {
    if file_id.is_none()
        && chunk_id.is_none()
        && topic_id.is_none()
        && symbol_id.is_none()
        && project_id.is_none()
    {
        return Err(sqlx::Error::Protocol(
            "memory_anchor_entity: at least one of file_id/chunk_id/topic_id/symbol_id/project_id \
             is required"
                .into(),
        ));
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO memory_code_anchor
            (entity_id, file_id, chunk_id, topic_id, symbol_id, project_id, anchor_type)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING id",
    )
    .bind(entity_id)
    .bind(file_id)
    .bind(chunk_id)
    .bind(topic_id)
    .bind(symbol_id)
    .bind(project_id)
    .bind(anchor_type)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

pub async fn memory_unanchor_entity(pool: &PgPool, anchor_id: i64) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("DELETE FROM memory_code_anchor WHERE id = $1")
        .bind(anchor_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn memory_find_code_for_entity(
    pool: &PgPool,
    entity_id: i64,
    anchor_type: Option<&str>,
) -> Result<Vec<MemoryCodeAnchorRow>, sqlx::Error> {
    sqlx::query_as::<_, MemoryCodeAnchorRow>(
        "SELECT id, entity_id, file_id, chunk_id, topic_id, symbol_id, project_id, anchor_type, created_at
         FROM memory_code_anchor
         WHERE entity_id = $1
           AND ($2::text IS NULL OR anchor_type = $2)
         ORDER BY created_at DESC",
    )
    .bind(entity_id)
    .bind(anchor_type)
    .fetch_all(pool)
    .await
}

pub async fn memory_find_entities_for_code(
    pool: &PgPool,
    file_id: Option<i64>,
    chunk_id: Option<i64>,
    topic_id: Option<i64>,
    symbol_id: Option<i64>,
    project_id: Option<i32>,
) -> Result<Vec<MemoryCodeAnchorRow>, sqlx::Error> {
    let provided = [
        file_id.is_some(),
        chunk_id.is_some(),
        topic_id.is_some(),
        symbol_id.is_some(),
        project_id.is_some(),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    if provided != 1 {
        return Err(sqlx::Error::Protocol(
            "memory_find_entities_for_code: pass exactly one of file_id, chunk_id, topic_id, \
             symbol_id, project_id"
                .into(),
        ));
    }
    sqlx::query_as::<_, MemoryCodeAnchorRow>(
        "SELECT id, entity_id, file_id, chunk_id, topic_id, symbol_id, project_id, anchor_type, created_at
         FROM memory_code_anchor
         WHERE ($1::bigint IS NOT NULL AND file_id  = $1)
            OR ($2::bigint IS NOT NULL AND chunk_id = $2)
            OR ($3::bigint IS NOT NULL AND topic_id = $3)
            OR ($4::bigint IS NOT NULL AND symbol_id = $4)
            OR ($5::int    IS NOT NULL AND project_id = $5)
         ORDER BY created_at DESC",
    )
    .bind(file_id)
    .bind(chunk_id)
    .bind(topic_id)
    .bind(symbol_id)
    .bind(project_id)
    .fetch_all(pool)
    .await
}

// ============================================================================
// Memory-server Phase 6: hierarchical + graph-enhanced retrieval queries
// ============================================================================

/// Phase 6.3 result row from `memory_unified_nodes` matview.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct UnifiedNodeHit {
    pub node_id: String,
    pub node_type: String,
    pub label: String,
    pub importance: f64,
    pub similarity: Option<f64>,
}

/// Phase 6.3: vector-similarity search over the unified-nodes matview.
/// Optionally filter to a subset of node_type strings.
pub async fn memory_unified_search(
    pool: &PgPool,
    embedding: &[f32],
    node_types: Option<&[String]>,
    limit: i32,
    ef_search: i32,
) -> Result<Vec<UnifiedNodeHit>, sqlx::Error> {
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "memory_unified_search: expected 1024d embedding, got {}",
            embedding.len()
        )));
    }
    let v = pgvector::Vector::from(embedding.to_vec());
    let mut tx = pool.begin().await?;
    sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", ef_search))
        .execute(&mut *tx)
        .await?;
    let rows = sqlx::query_as::<_, UnifiedNodeHit>(
        "SELECT node_id, node_type, label, importance,
                1 - (embedding <=> $1) AS similarity
         FROM memory_unified_nodes
         WHERE embedding IS NOT NULL
           AND ($2::text[] IS NULL OR node_type = ANY($2))
         ORDER BY embedding <=> $1
         LIMIT $3",
    )
    .bind(&v)
    .bind(node_types)
    .bind(limit.clamp(1, 200))
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}

/// Refresh a unified-graph materialized view `CONCURRENTLY` under a raised
/// per-statement timeout.
///
/// The daemon-wide `statement_timeout` (30 s, set in `db::pool::after_connect`)
/// is too short for these large UNION-ALL matviews (18+ arms over `file_chunks`,
/// embeddings, etc.), so a bare `REFRESH` cancels at 30 s (SQLSTATE 57014) and
/// the matview goes stale. This lifts the timeout via `SET LOCAL` inside a
/// transaction — the established heavy-cron idiom (cf. `cron::graph_analysis`,
/// `queries::similarity`). `REFRESH … CONCURRENTLY` **is** legal inside a
/// transaction (verified against PostgreSQL; it is `CREATE INDEX CONCURRENTLY`
/// that is not — an earlier comment here had it backwards). The `pgmcp:heavy:`
/// `application_name` lets the graceful-shutdown sweep
/// (`db::admin::terminate_heavy_backends`) find the backend. `view` is always an
/// internal string literal, so the interpolation carries no injection risk.
///
/// `CONCURRENTLY` keeps graph-RAG reads (neighbors / PPR / RAPTOR) unblocked
/// during the refresh; it relies on the matview's unique index and on the
/// boot-time `CREATE … WITH DATA` population, so the first refresh has data to
/// diff against.
async fn refresh_matview_concurrently(
    pool: &PgPool,
    view: &str,
    job_tag: &str,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    // 10 minutes: generous headroom over the ~tens-of-seconds the refresh needs,
    // while still bounding a runaway. Matches `cron::graph_analysis`'s literal.
    sqlx::query("SET LOCAL statement_timeout = '600s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query(&format!(
        "SET LOCAL application_name = 'pgmcp:heavy:{job_tag}'"
    ))
    .execute(&mut *tx)
    .await?;
    sqlx::query(&format!("REFRESH MATERIALIZED VIEW CONCURRENTLY {view}"))
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

/// Phase 6.3: refresh the unified **nodes** matview. A UNION ALL over indexed
/// tables; relies on `idx_memory_unified_nodes_uq (node_id)`. Called from the
/// `memory-graph-refresh` / `memory-concepts` crons or on-demand.
pub async fn refresh_memory_unified_nodes(pool: &PgPool) -> Result<(), sqlx::Error> {
    refresh_matview_concurrently(pool, "memory_unified_nodes", "memory-graph-refresh").await
}

/// Unified-graph (Stage 2): refresh the materialized **edges** view. A UNION ALL
/// over indexed tables; relies on `idx_memory_unified_edges_uq (from_id, to_id,
/// edge_type)` (the outer GROUP BY in `MEMORY_UNIFIED_EDGES_SQL` makes that key
/// unique). Called from the `memory-graph-refresh` / `memory-concepts` /
/// `trajectory-similarity` crons or on-demand.
pub async fn refresh_memory_unified_edges(pool: &PgPool) -> Result<(), sqlx::Error> {
    refresh_matview_concurrently(pool, "memory_unified_edges", "memory-graph-refresh").await
}

/// Phase 6.3: BFS neighbors of a typed node over `memory_unified_edges`.
/// Returns the reachable nodes up to `depth` plus the edges that connect
/// them.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct UnifiedNeighborNode {
    pub node_id: String,
    pub node_type: String,
    pub depth: i32,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct UnifiedEdge {
    pub from_id: String,
    pub from_type: String,
    pub to_id: String,
    pub to_type: String,
    pub edge_type: String,
    pub weight: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UnifiedNeighborhood {
    pub seed: String,
    pub nodes: Vec<UnifiedNeighborNode>,
    pub edges: Vec<UnifiedEdge>,
}

/// Resolve a friendly graph node reference `<node_type>:<key>` to the composite
/// `node_id` used by `memory_unified_nodes` (`<node_type>:<pk>`). Numeric keys
/// (and `agent`, whose key *is* its free-text id) pass through unchanged; other
/// types are looked up by their natural key (file path, project/topic name,
/// work-item public_id, experiment slug, commit hash, symbol name). Returns
/// `None` when no row matches. Used by the `graph_neighbors` tool.
pub async fn resolve_graph_node_id(
    pool: &PgPool,
    node_type: &str,
    key: &str,
) -> Result<Option<String>, sqlx::Error> {
    if node_type == "agent" || key.parse::<i64>().is_ok() {
        return Ok(Some(format!("{node_type}:{key}")));
    }
    let id: Option<i64> =
        match node_type {
            "file" => {
                sqlx::query_scalar(
                    "SELECT id FROM indexed_files WHERE relative_path = $1 OR path = $1 LIMIT 1",
                )
                .bind(key)
                .fetch_optional(pool)
                .await?
            }
            "project" => {
                sqlx::query_scalar(
                    "SELECT id::bigint FROM projects WHERE name = $1 OR path = $1 LIMIT 1",
                )
                .bind(key)
                .fetch_optional(pool)
                .await?
            }
            "work_item" => {
                sqlx::query_scalar("SELECT id FROM work_items WHERE public_id = $1 LIMIT 1")
                    .bind(key)
                    .fetch_optional(pool)
                    .await?
            }
            "experiment" => {
                sqlx::query_scalar(
                    "SELECT id FROM experiments WHERE slug = $1 AND valid_to IS NULL LIMIT 1",
                )
                .bind(key)
                .fetch_optional(pool)
                .await?
            }
            "topic" => {
                sqlx::query_scalar("SELECT id FROM code_topics WHERE label = $1 LIMIT 1")
                    .bind(key)
                    .fetch_optional(pool)
                    .await?
            }
            "commit" => sqlx::query_scalar(
                "SELECT id FROM git_commits WHERE commit_hash = $1 OR commit_hash LIKE $1 || '%' \
                 ORDER BY author_date DESC LIMIT 1",
            )
            .bind(key)
            .fetch_optional(pool)
            .await?,
            "symbol" => {
                sqlx::query_scalar("SELECT id FROM file_symbols WHERE name = $1 LIMIT 1")
                    .bind(key)
                    .fetch_optional(pool)
                    .await?
            }
            _ => None,
        };
    Ok(id.map(|pk| format!("{node_type}:{pk}")))
}

pub async fn memory_neighbors(
    pool: &PgPool,
    node_id: &str,
    depth: i32,
    edge_filter: Option<&str>,
    max_nodes: i32,
) -> Result<UnifiedNeighborhood, sqlx::Error> {
    let depth_cap = depth.clamp(1, 4);
    let node_cap = max_nodes.clamp(1, 500);
    let rows: Vec<(String, String, i32)> = sqlx::query_as(
        "WITH RECURSIVE frontier(node_id, node_type, depth) AS (
             SELECT node_id, node_type, 0::int
             FROM memory_unified_nodes
             WHERE node_id = $1
             UNION
             SELECT CASE WHEN e.from_id = f.node_id THEN e.to_id ELSE e.from_id END,
                    CASE WHEN e.from_id = f.node_id THEN e.to_type ELSE e.from_type END,
                    f.depth + 1
             FROM frontier f
             JOIN memory_unified_edges e
                  ON  (e.from_id = f.node_id OR e.to_id = f.node_id)
                  AND ($2::text IS NULL OR e.edge_type = $2)
             WHERE f.depth < $3
         )
         SELECT node_id, node_type, MIN(depth)::int AS depth
         FROM frontier
         GROUP BY node_id, node_type
         ORDER BY MIN(depth), node_id
         LIMIT $4",
    )
    .bind(node_id)
    .bind(edge_filter)
    .bind(depth_cap)
    .bind(node_cap)
    .fetch_all(pool)
    .await?;

    let nodes: Vec<UnifiedNeighborNode> = rows
        .into_iter()
        .map(|(id, t, d)| UnifiedNeighborNode {
            node_id: id,
            node_type: t,
            depth: d,
        })
        .collect();
    let node_ids: Vec<String> = nodes.iter().map(|n| n.node_id.clone()).collect();
    let edges: Vec<UnifiedEdge> = if node_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            "SELECT from_id, from_type, to_id, to_type, edge_type, weight
             FROM memory_unified_edges
             WHERE from_id = ANY($1) AND to_id = ANY($1)
               AND ($2::text IS NULL OR edge_type = $2)",
        )
        .bind(&node_ids)
        .bind(edge_filter)
        .fetch_all(pool)
        .await?
    };

    Ok(UnifiedNeighborhood {
        seed: node_id.to_string(),
        nodes,
        edges,
    })
}

/// Phase 6.4 PathRAG: ranked paths through the unified graph. Seeds
/// from `memory_unified_search`, then BFS-expands within
/// `max_hops`, ranks by a composite (cosine of last-node vs query,
/// minus hop-length penalty, plus edge-weight product), and prunes
/// near-duplicate paths via Jaccard overlap on the node-set.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryPath {
    /// node_ids in order, starting from the seed.
    pub nodes: Vec<String>,
    pub edge_types: Vec<String>,
    pub score: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryPathSearchResult {
    pub seeds: Vec<String>,
    pub paths: Vec<MemoryPath>,
    /// Paths considered before pruning (telemetry).
    pub considered: i64,
    pub pruned: i64,
}

pub async fn memory_path_search(
    pool: &PgPool,
    embedding: &[f32],
    seed_node_types: Option<&[String]>,
    target_node_types: Option<&[String]>,
    max_hops: i32,
    k: i32,
    prune_jaccard: f64,
    ef_search: i32,
    // Stage 5b: optional point-in-time filter (only edges valid at `as_of`) and
    // recency half-life (days) folded into the per-edge weight so recent edges
    // score higher. `as_of = None` ⇒ current graph; timeless structural edges
    // (NULL validity) are always included and never decayed.
    as_of: Option<DateTime<Utc>>,
    half_life_days: f64,
) -> Result<MemoryPathSearchResult, sqlx::Error> {
    let hop_cap = max_hops.clamp(1, 5);
    let k = k.clamp(1, 100);

    // 1. Seed by top-k semantic over unified-nodes (k seeds = k).
    let seeds =
        memory_unified_search(pool, embedding, seed_node_types, k.max(5), ef_search).await?;
    if seeds.is_empty() {
        return Ok(MemoryPathSearchResult {
            seeds: Vec::new(),
            paths: Vec::new(),
            considered: 0,
            pruned: 0,
        });
    }
    let seed_ids: Vec<String> = seeds.iter().map(|s| s.node_id.clone()).collect();

    // 2. BFS-expand and emit complete paths. Bound output via hop_cap
    // (worst-case branching is bounded since each step joins through
    // `memory_unified_edges`, which is already capped by the membership
    // and code_anchor filters). LIMIT 400 keeps it sane.
    let rows: Vec<(String, String, String, String, String, f64, i32)> = sqlx::query_as(
        "WITH RECURSIVE walk(start_id, current_id, current_type,
                              last_edge, last_to_type, weight_product, hops,
                              path_nodes, path_edges) AS (
             SELECT s.node_id, s.node_id, s.node_type,
                    ''::text, s.node_type, 1.0::float8, 0::int,
                    ARRAY[s.node_id], ARRAY[]::text[]
             FROM memory_unified_nodes s
             WHERE s.node_id = ANY($1)
             UNION
             SELECT w.start_id,
                    CASE WHEN e.from_id = w.current_id THEN e.to_id ELSE e.from_id END,
                    CASE WHEN e.from_id = w.current_id THEN e.to_type ELSE e.from_type END,
                    e.edge_type,
                    CASE WHEN e.from_id = w.current_id THEN e.to_type ELSE e.from_type END,
                    w.weight_product * e.weight
                        * (CASE WHEN e.valid_from IS NULL THEN 1.0
                                ELSE exp(-0.6931471805599453
                                         * GREATEST(0.0, EXTRACT(EPOCH FROM (now() - e.valid_from)) / 86400.0)
                                         / GREATEST($5::float8, 0.001)) END),
                    w.hops + 1,
                    w.path_nodes || (CASE WHEN e.from_id = w.current_id THEN e.to_id ELSE e.from_id END),
                    w.path_edges || e.edge_type
             FROM walk w
             JOIN memory_unified_edges e
                  ON e.from_id = w.current_id OR e.to_id = w.current_id
             WHERE w.hops < $2
               AND NOT (
                   CASE WHEN e.from_id = w.current_id THEN e.to_id ELSE e.from_id END
                       = ANY(w.path_nodes)
               )
               AND ($4::timestamptz IS NULL OR e.valid_from IS NULL
                    OR (e.valid_from <= $4 AND (e.valid_to IS NULL OR e.valid_to > $4)))
         )
         SELECT start_id, current_id, current_type, last_edge, last_to_type,
                weight_product, hops
         FROM walk
         WHERE hops > 0
           AND ($3::text[] IS NULL OR current_type = ANY($3))
         ORDER BY hops, weight_product DESC
         LIMIT 400",
    )
    .bind(&seed_ids)
    .bind(hop_cap)
    .bind(target_node_types)
    .bind(as_of)
    .bind(half_life_days)
    .fetch_all(pool)
    .await?;
    let considered = rows.len() as i64;

    // We need the actual path nodes to render paths cleanly. Re-query
    // a richer set including the path_nodes / path_edges arrays.
    let path_rows: Vec<(Vec<String>, Vec<String>, f64, i32)> = sqlx::query_as(
        "WITH RECURSIVE walk(start_id, current_id, weight_product, hops,
                              path_nodes, path_edges) AS (
             SELECT s.node_id, s.node_id, 1.0::float8, 0::int,
                    ARRAY[s.node_id], ARRAY[]::text[]
             FROM memory_unified_nodes s
             WHERE s.node_id = ANY($1)
             UNION
             SELECT w.start_id,
                    CASE WHEN e.from_id = w.current_id THEN e.to_id ELSE e.from_id END,
                    w.weight_product * e.weight
                        * (CASE WHEN e.valid_from IS NULL THEN 1.0
                                ELSE exp(-0.6931471805599453
                                         * GREATEST(0.0, EXTRACT(EPOCH FROM (now() - e.valid_from)) / 86400.0)
                                         / GREATEST($5::float8, 0.001)) END),
                    w.hops + 1,
                    w.path_nodes || (CASE WHEN e.from_id = w.current_id THEN e.to_id ELSE e.from_id END),
                    w.path_edges || e.edge_type
             FROM walk w
             JOIN memory_unified_edges e
                  ON e.from_id = w.current_id OR e.to_id = w.current_id
             WHERE w.hops < $2
               AND NOT (
                   CASE WHEN e.from_id = w.current_id THEN e.to_id ELSE e.from_id END
                       = ANY(w.path_nodes)
               )
               AND ($4::timestamptz IS NULL OR e.valid_from IS NULL
                    OR (e.valid_from <= $4 AND (e.valid_to IS NULL OR e.valid_to > $4)))
         ),
         filtered AS (
             SELECT path_nodes, path_edges, weight_product, hops
             FROM walk
             JOIN memory_unified_nodes n ON n.node_id = walk.current_id
             WHERE hops > 0
               AND ($3::text[] IS NULL OR n.node_type = ANY($3))
         )
         SELECT path_nodes, path_edges, weight_product, hops
         FROM filtered
         ORDER BY weight_product DESC, hops
         LIMIT 200",
    )
    .bind(&seed_ids)
    .bind(hop_cap)
    .bind(target_node_types)
    .bind(as_of)
    .bind(half_life_days)
    .fetch_all(pool)
    .await?;

    // 3. Score each path. Composite: weight_product − 0.1·hops (we
    // don't have the query embedding cosine for intermediate nodes
    // cheaply; the seed cosine is baked into `seeds[i].similarity`,
    // which we incorporate by weighting the start-seed similarity).
    let seed_sim_map: std::collections::HashMap<String, f64> = seeds
        .iter()
        .map(|s| (s.node_id.clone(), s.similarity.unwrap_or(0.0)))
        .collect();

    let mut scored: Vec<MemoryPath> = Vec::with_capacity(path_rows.len());
    for (nodes, edges, weight_product, hops) in path_rows {
        let seed_sim = nodes
            .first()
            .and_then(|id| seed_sim_map.get(id))
            .copied()
            .unwrap_or(0.0);
        let score = 0.6 * seed_sim + 0.3 * weight_product - 0.1 * (hops as f64);
        scored.push(MemoryPath {
            nodes,
            edge_types: edges,
            score,
        });
    }
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // 4. PathRAG flow-style pruning: drop paths whose node-set
    // overlaps a kept path's node-set above `prune_jaccard`.
    let mut kept: Vec<MemoryPath> = Vec::with_capacity(k as usize);
    let mut pruned = 0_i64;
    for p in scored {
        let pset: std::collections::BTreeSet<&String> = p.nodes.iter().collect();
        let mut overlaps = false;
        for q in &kept {
            let qset: std::collections::BTreeSet<&String> = q.nodes.iter().collect();
            let inter = pset.intersection(&qset).count() as f64;
            let union = pset.union(&qset).count() as f64;
            let jacc = if union > 0.0 { inter / union } else { 0.0 };
            if jacc >= prune_jaccard {
                overlaps = true;
                pruned += 1;
                break;
            }
        }
        if !overlaps {
            kept.push(p);
            if kept.len() as i32 >= k {
                break;
            }
        }
    }

    Ok(MemoryPathSearchResult {
        seeds: seed_ids,
        paths: kept,
        considered,
        pruned,
    })
}

/// HippoRAG-style PPR result row over the unified graph (Stage 6: any node
/// type, keyed by composite `node_id`, not just memory entities).
#[derive(Debug, Clone, serde::Serialize)]
pub struct PprHit {
    pub node_id: String,
    pub node_type: String,
    pub label: String,
    pub ppr_score: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PprSearchResult {
    pub seeds: Vec<String>,
    pub hits: Vec<PprHit>,
}

/// Stage 6: HippoRAG-style Personalized PageRank over the **whole unified
/// graph** (`memory_unified_edges`), spanning every record type. Seeds are the
/// top-k unified nodes by vector similarity to the query embedding; PPR then
/// diffuses from them across the heterogeneous edge set.
pub async fn memory_ppr_search(
    pool: &PgPool,
    embedding: &[f32],
    k: i32,
    alpha: f64,
    max_seeds: i32,
    ef_search: i32,
) -> Result<PprSearchResult, sqlx::Error> {
    // 1. Seeds: top-k unified-graph nodes by vector similarity (string node_ids),
    // spanning every record type — not just memory entities (Stage 6).
    let seeds =
        memory_unified_search(pool, embedding, None, max_seeds.clamp(1, 100), ef_search).await?;
    let seed_ids: Vec<String> = seeds.iter().map(|s| s.node_id.clone()).collect();
    if seed_ids.is_empty() {
        return Ok(PprSearchResult {
            seeds: Vec::new(),
            hits: Vec::new(),
        });
    }

    // 2. Load the unified edge graph (string node_ids) into adjacency.
    let edges: Vec<(String, String, f64)> =
        sqlx::query_as("SELECT from_id, to_id, weight FROM memory_unified_edges")
            .fetch_all(pool)
            .await?;
    let mut node_to_idx: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut idx_to_node: Vec<String> = Vec::new();
    let mut adjacency: Vec<Vec<(usize, f64)>> = Vec::new();
    let ensure_idx = |n: &str,
                      node_to_idx: &mut std::collections::HashMap<String, usize>,
                      idx_to_node: &mut Vec<String>,
                      adj: &mut Vec<Vec<(usize, f64)>>|
     -> usize {
        if let Some(&idx) = node_to_idx.get(n) {
            return idx;
        }
        let idx = idx_to_node.len();
        node_to_idx.insert(n.to_string(), idx);
        idx_to_node.push(n.to_string());
        adj.push(Vec::new());
        idx
    };
    for (from, to, w) in edges {
        let fi = ensure_idx(&from, &mut node_to_idx, &mut idx_to_node, &mut adjacency);
        let ti = ensure_idx(&to, &mut node_to_idx, &mut idx_to_node, &mut adjacency);
        adjacency[fi].push((ti, w));
        adjacency[ti].push((fi, w));
    }
    // Ensure every seed is a node (some may have no edges; still valid restart
    // nodes for the PPR diffusion).
    for sid in &seed_ids {
        ensure_idx(sid, &mut node_to_idx, &mut idx_to_node, &mut adjacency);
    }

    let n = idx_to_node.len();
    if n == 0 {
        return Ok(PprSearchResult {
            seeds: seed_ids,
            hits: Vec::new(),
        });
    }

    // 3. Power iteration: PR(v) = α · (Σ PR(u)·w(u,v)/d(u)) + (1-α) · r(v),
    // where r is the restart distribution concentrated on the seeds.
    let mut restart = vec![0.0_f64; n];
    let seed_indices: Vec<usize> = seed_ids
        .iter()
        .filter_map(|id| node_to_idx.get(id).copied())
        .collect();
    let restart_mass = 1.0 / seed_indices.len() as f64;
    for &si in &seed_indices {
        restart[si] = restart_mass;
    }
    let mut rank = restart.clone();
    // Precompute row sums for normalization.
    let row_sums: Vec<f64> = adjacency
        .iter()
        .map(|row| row.iter().map(|(_, w)| *w).sum::<f64>().max(1e-12))
        .collect();

    let iters = 25_usize;
    for _ in 0..iters {
        let mut next = vec![0.0_f64; n];
        for (u, neighbors) in adjacency.iter().enumerate() {
            if rank[u] == 0.0 {
                continue;
            }
            let share = rank[u] / row_sums[u];
            for (v, w) in neighbors {
                next[*v] += alpha * share * *w;
            }
        }
        for i in 0..n {
            next[i] += (1.0 - alpha) * restart[i];
        }
        rank = next;
    }

    // 4. Take top-k by PR score; enrich from memory_unified_nodes (type + label).
    let mut ranked: Vec<(usize, f64)> = rank.iter().enumerate().map(|(i, r)| (i, *r)).collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(k.clamp(1, 200) as usize);

    let top_ids: Vec<String> = ranked
        .iter()
        .map(|(i, _)| idx_to_node[*i].clone())
        .collect();
    let mut hits: Vec<PprHit> = Vec::with_capacity(top_ids.len());
    if !top_ids.is_empty() {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT node_id, node_type, label
             FROM memory_unified_nodes
             WHERE node_id = ANY($1)",
        )
        .bind(&top_ids)
        .fetch_all(pool)
        .await?;
        let score_map: std::collections::HashMap<String, f64> = ranked
            .iter()
            .map(|(i, r)| (idx_to_node[*i].clone(), *r))
            .collect();
        for (node_id, node_type, label) in rows {
            let ppr_score = *score_map.get(&node_id).unwrap_or(&0.0);
            hits.push(PprHit {
                node_id,
                node_type,
                label,
                ppr_score,
            });
        }
        hits.sort_by(|a, b| {
            b.ppr_score
                .partial_cmp(&a.ppr_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    Ok(PprSearchResult {
        seeds: seed_ids,
        hits,
    })
}

/// Phase 6.1 RAPTOR query result.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct RaptorHit {
    pub node_id: i64,
    pub level: i32,
    pub label: String,
    pub similarity: Option<f64>,
}

/// Phase 6.1: query against `memory_summary_tree`. Returns top-k
/// summary nodes at each requested level (or all levels), ranked
/// by cosine over `summary_embedding`. Useful for "thematic"
/// queries that span many observations.
pub async fn memory_raptor_search(
    pool: &PgPool,
    embedding: &[f32],
    scope_id: Option<i64>,
    levels: Option<&[i32]>,
    k: i32,
    ef_search: i32,
) -> Result<Vec<RaptorHit>, sqlx::Error> {
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "memory_raptor_search: expected 1024d embedding, got {}",
            embedding.len()
        )));
    }
    let v = pgvector::Vector::from(embedding.to_vec());
    let mut tx = pool.begin().await?;
    sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", ef_search))
        .execute(&mut *tx)
        .await?;
    let rows = sqlx::query_as::<_, RaptorHit>(
        "SELECT id AS node_id, level,
                COALESCE(summary_text, '<leaf>') AS label,
                1 - (summary_embedding <=> $1) AS similarity
         FROM memory_summary_tree
         WHERE summary_embedding IS NOT NULL
           AND ($2::bigint IS NULL OR scope_id = $2)
           AND ($3::int[] IS NULL OR level = ANY($3))
         ORDER BY summary_embedding <=> $1
         LIMIT $4",
    )
    .bind(&v)
    .bind(scope_id)
    .bind(levels)
    .bind(k.clamp(1, 200))
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}
