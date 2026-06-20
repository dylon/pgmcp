//! Remaining functor tools (ADR-028, item 4): `effect_functor` (the Call →
//! effect-set monoid), `naturality_gap` (import functor vs semantic functor
//! divergence), and `colimit_view` (the unified graph as a colimit of its
//! per-source diagrams). All read existing tables/matviews.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::{ColimitViewParams, EffectFunctorParams, NaturalityGapParams};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

/// `effect_functor` — the image of the Call category under the effect functor:
/// the effect monoid (generators = distinct effects, ∪ = composition) plus the
/// most effectful symbols and their effect sets.
pub async fn tool_effect_functor(
    ctx: &SystemContext,
    params: EffectFunctorParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let limit = params.limit.unwrap_or(50).clamp(1, 500);

    // Monoid generators: distinct effects + how many symbols carry each.
    let generators = sqlx::query_as::<_, (String, i64)>(
        "SELECT se.effect, COUNT(DISTINCT se.symbol_id)
           FROM symbol_effects se
           JOIN file_symbols s ON s.id = se.symbol_id
           JOIN indexed_files f ON f.id = s.file_id
          WHERE f.project_id = $1
          GROUP BY se.effect
          ORDER BY 2 DESC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("effects: {e}"), None))?;

    // Most effectful symbols (largest image under the functor).
    let symbols = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT s.name, f.relative_path, COUNT(*)
           FROM symbol_effects se
           JOIN file_symbols s ON s.id = se.symbol_id
           JOIN indexed_files f ON f.id = s.file_id
          WHERE f.project_id = $1
          GROUP BY s.name, f.relative_path
          ORDER BY 3 DESC
          LIMIT $2",
    )
    .bind(project_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("symbols: {e}"), None))?;

    json_result(&json!({
        "project": params.project,
        "monoid_generators": generators.iter().map(|(e, n)| json!({"effect": e, "symbol_count": n})).collect::<Vec<_>>(),
        "generator_count": generators.len(),
        "most_effectful_symbols": symbols.iter().map(|(n, p, c)| json!({"symbol": n, "path": p, "effect_count": c})).collect::<Vec<_>>(),
        "note": "Effects form a monoid (∪ = composition, identity = ∅). The functor Call → (Effects, ∪) \
    maps each function to its effect set; a sound caller's effects should cover its callees' (effect propagation).",
        "guidance": if generators.is_empty() {
            Some("no effects recorded — run the symbol-extraction cron (shadow-ASR effects)")
        } else { None },
    }))
}

/// `naturality_gap` — where the IMPORT functor and the SEMANTIC functor disagree:
/// file pairs that are structurally coupled (an import edge) yet semantically
/// distant (low averaged-embedding cosine). High structural + low semantic =
/// architectural erosion / a leaky abstraction the natural transformation
/// doesn't preserve.
pub async fn tool_naturality_gap(
    ctx: &SystemContext,
    params: NaturalityGapParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let threshold = params.threshold.unwrap_or(0.5);
    let limit = params.limit.unwrap_or(50).clamp(1, 500);

    // File vectors = mean of chunk embeddings; cosine over import edges.
    let rows = sqlx::query_as::<_, (String, String, f64)>(
        "WITH fv AS (
             SELECT file_id, AVG(embedding_v2) AS v
               FROM file_chunks WHERE embedding_v2 IS NOT NULL GROUP BY file_id
         )
         SELECT sf.relative_path, tf.relative_path, 1.0 - (a.v <=> b.v) AS sim
           FROM code_graph_edges e
           JOIN fv a ON a.file_id = e.source_file_id
           JOIN fv b ON b.file_id = e.target_file_id
           JOIN indexed_files sf ON sf.id = e.source_file_id
           JOIN indexed_files tf ON tf.id = e.target_file_id
          WHERE e.project_id = $1 AND e.edge_type = 'import'
            AND e.target_file_id IS NOT NULL
            AND (1.0 - (a.v <=> b.v)) < $2
          ORDER BY sim ASC
          LIMIT $3",
    )
    .bind(project_id)
    .bind(threshold)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("naturality: {e}"), None))?;

    json_result(&json!({
        "project": params.project,
        "threshold": threshold,
        "gap_count": rows.len(),
        "gaps": rows.iter().map(|(s, t, sim)| json!({"from": s, "to": t, "semantic_similarity": sim})).collect::<Vec<_>>(),
        "note": "Import edges whose endpoints are semantically distant (cosine < threshold): the import \
    and semantic functors disagree — structurally coupled but conceptually unrelated (erosion / leaky \
    abstraction). Co-change is the third functor; add it when a co-change table is materialized.",
        "guidance": if rows.is_empty() {
            Some("no gaps (or no file embeddings / import edges) — run graph-analysis + embedding crons")
        } else { None },
    }))
}

/// `colimit_view` — the unified memory/code graph (`memory_unified_edges`) as a
/// COLIMIT of its per-source diagrams: the component breakdown (each
/// (from_type, edge_type, to_type) is a diagram arm glued into the colimit) plus
/// the node-type tally (the colimit's objects).
pub async fn tool_colimit_view(
    ctx: &SystemContext,
    params: ColimitViewParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(100).clamp(1, 1000);

    let components = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT from_type, edge_type, to_type, COUNT(*)
           FROM memory_unified_edges
          GROUP BY from_type, edge_type, to_type
          ORDER BY 4 DESC
          LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("edges: {e}"), None))?;

    let objects = sqlx::query_as::<_, (String, i64)>(
        "SELECT node_type, COUNT(*) FROM memory_unified_nodes GROUP BY node_type ORDER BY 2 DESC",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    json_result(&json!({
        "objects": objects.iter().map(|(t, n)| json!({"node_type": t, "count": n})).collect::<Vec<_>>(),
        "diagram_components": components.iter().map(|(f, e, t, n)| json!({"from_type": f, "edge_type": e, "to_type": t, "count": n})).collect::<Vec<_>>(),
        "component_count": components.len(),
        "note": "The unified graph is the COLIMIT of its per-source diagrams: each (from_type, edge_type, \
    to_type) arm is glued in, with shared nodes identified across sources. This is the formal reading of \
    memory_unified_edges / _nodes.",
        "guidance": if components.is_empty() {
            Some("unified graph empty — refresh the memory_unified matviews (graph crons)")
        } else { None },
    }))
}
