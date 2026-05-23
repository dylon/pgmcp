//! Effect-tag queries over `symbol_effects`.
//!
//! Replaces the regex-over-content patterns in Phase 5/6 security and
//! concurrency tools (taint_analysis, blocking_in_async, panic_paths,
//! unsafe_clusters, etc.) with structured JOINs.

use sqlx::PgPool;
use std::collections::{HashMap, HashSet};

/// All symbols in `project_id` with the given effect, returned as
/// `(symbol_id, file_id, name, scope_path)` tuples.
pub async fn symbols_with_effect(
    pool: &PgPool,
    project_id: i32,
    effect: &str,
) -> Result<Vec<(i64, i64, String, Option<String>)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT fs.id, fs.file_id, fs.name, fs.scope_path
         FROM symbol_effects se
         JOIN file_symbols fs ON fs.id = se.symbol_id
         JOIN indexed_files f ON f.id = fs.file_id
         WHERE f.project_id = $1
           AND se.effect = $2
         ORDER BY fs.file_id, fs.start_line",
    )
    .bind(project_id)
    .bind(effect)
    .fetch_all(pool)
    .await
}

/// Full effect set for a single symbol.
pub async fn effect_set_for(pool: &PgPool, symbol_id: i64) -> Result<HashSet<String>, sqlx::Error> {
    let rows: Vec<String> =
        sqlx::query_scalar("SELECT effect FROM symbol_effects WHERE symbol_id = $1")
            .bind(symbol_id)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().collect())
}

/// Count of symbols carrying each effect in a project. Used by scorecard
/// / tech-debt-burn-down style summaries.
pub async fn effect_counts(
    pool: &PgPool,
    project_id: i32,
) -> Result<HashMap<String, i64>, sqlx::Error> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT se.effect, COUNT(*)::int8
         FROM symbol_effects se
         JOIN file_symbols fs ON fs.id = se.symbol_id
         JOIN indexed_files f ON f.id = fs.file_id
         WHERE f.project_id = $1
         GROUP BY se.effect
         ORDER BY se.effect",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Forward-reachable effect set from a seed symbol: all effects observed
/// on any symbol transitively callable from the seed via resolved call
/// edges. The traversal is bounded by `max_depth` to keep work O(N·d).
///
/// Returns per-effect statistics: `(effect, count, min_depth)`.
#[derive(Debug, Clone)]
pub struct ReachStats {
    pub count: u32,
    pub min_depth: u32,
}

pub async fn effects_reachable_from(
    pool: &PgPool,
    seed_symbol_id: i64,
    max_depth: u32,
) -> Result<HashMap<String, ReachStats>, sqlx::Error> {
    // Build the resolved-edge subgraph starting from the seed. Each row in
    // `symbol_references` with `resolution_kind in (exact_in_file,
    // exact_via_import)` becomes an edge.
    //
    // Use a recursive CTE bounded by `max_depth`. PostgreSQL's recursive
    // CTE supports this directly.
    let rows: Vec<(String, i64, i32)> = sqlx::query_as(
        "WITH RECURSIVE walk(symbol_id, depth) AS (
            SELECT $1::int8, 0
            UNION
            SELECT sr.target_symbol_id, walk.depth + 1
            FROM walk
            JOIN symbol_references sr ON sr.source_symbol_id = walk.symbol_id
            WHERE sr.target_symbol_id IS NOT NULL
              AND sr.resolution_kind IN ('exact_in_file', 'exact_via_import')
              AND walk.depth < $2::int4
         )
         SELECT se.effect, COUNT(*)::int8, MIN(walk.depth)::int4
         FROM walk
         JOIN symbol_effects se ON se.symbol_id = walk.symbol_id
         GROUP BY se.effect",
    )
    .bind(seed_symbol_id)
    .bind(max_depth as i32)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(effect, count, min_depth)| {
            (
                effect,
                ReachStats {
                    count: count.max(0) as u32,
                    min_depth: min_depth.max(0) as u32,
                },
            )
        })
        .collect())
}

/// Symbols that have ANY of the given effects (union semantics).
/// Returns deduplicated `(symbol_id, file_id, name, scope_path)`.
pub async fn symbols_with_any_effect(
    pool: &PgPool,
    project_id: i32,
    effects: &[String],
) -> Result<Vec<(i64, i64, String, Option<String>)>, sqlx::Error> {
    if effects.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as(
        "SELECT DISTINCT fs.id, fs.file_id, fs.name, fs.scope_path
         FROM symbol_effects se
         JOIN file_symbols fs ON fs.id = se.symbol_id
         JOIN indexed_files f ON f.id = fs.file_id
         WHERE f.project_id = $1
           AND se.effect = ANY($2::text[])
         ORDER BY fs.file_id, fs.start_line",
    )
    .bind(project_id)
    .bind(effects)
    .fetch_all(pool)
    .await
}

/// Symbols that have ALL of the given effects (intersection semantics).
pub async fn symbols_with_all_effects(
    pool: &PgPool,
    project_id: i32,
    effects: &[String],
) -> Result<Vec<(i64, i64, String, Option<String>)>, sqlx::Error> {
    if effects.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as(
        "SELECT fs.id, fs.file_id, fs.name, fs.scope_path
         FROM file_symbols fs
         JOIN indexed_files f ON f.id = fs.file_id
         WHERE f.project_id = $1
           AND (
               SELECT COUNT(DISTINCT effect)
                 FROM symbol_effects se
                WHERE se.symbol_id = fs.id
                  AND se.effect = ANY($2::text[])
           ) = $3::int8
         ORDER BY fs.file_id, fs.start_line",
    )
    .bind(project_id)
    .bind(effects)
    .bind(effects.len() as i64)
    .fetch_all(pool)
    .await
}
