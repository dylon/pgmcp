//! Further topic-model applications (ADR-029, item 14): topic-scoped semantic
//! search (#4) and a quality-trajectory forecast over the architecture dimension
//! the topic model feeds (#11). Both reuse existing data (chunk_topic_assignments
//! + chunk embeddings; quality_report_history + `crate::quality::forecast`).

use chrono::{DateTime, Utc};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::{
    DocCodeTopicAlignmentParams, TopicExperimentMapParams, TopicQualityForecastParams,
    TopicScopedSearchParams,
};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::quality::forecast;

fn bad(msg: &str) -> McpError {
    McpError::invalid_params(msg.to_string(), None)
}

/// #4 — semantic search restricted to one topic's chunks.
pub async fn tool_topic_scoped_search(
    ctx: &SystemContext,
    params: TopicScopedSearchParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    if params.query.trim().is_empty() {
        return Err(bad("query must be non-empty"));
    }
    let limit = params.limit.unwrap_or(20).clamp(1, 100);

    let topic_id: i32 = match (params.topic_id, params.topic_label.as_deref()) {
        (Some(id), _) => id as i32,
        (None, Some(label)) => sqlx::query_scalar::<_, i32>(
            "SELECT id FROM code_topics WHERE label ILIKE '%' || $1 || '%'
              ORDER BY chunk_count DESC LIMIT 1",
        )
        .bind(label)
        .fetch_optional(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("topic lookup: {e}"), None))?
        .ok_or_else(|| bad("no topic matching topic_label"))?,
        (None, None) => return Err(bad("provide topic_id or topic_label")),
    };

    let qvec = pgvector::Vector::from(
        ctx.embed()
            .embed_query(&params.query)
            .await
            .map_err(|e| McpError::internal_error(format!("embed: {e}"), None))?,
    );

    let rows = sqlx::query_as::<_, (i64, String, i32, i32, String, f64)>(
        "SELECT fc.id, f.relative_path, fc.start_line, fc.end_line,
                left(fc.content, 200), 1.0 - (fc.embedding_v2 <=> $1) AS sim
           FROM file_chunks fc
           JOIN chunk_topic_assignments cta ON cta.chunk_id = fc.id
           JOIN indexed_files f ON f.id = fc.file_id
          WHERE cta.topic_id = $2 AND fc.embedding_v2 IS NOT NULL
          ORDER BY fc.embedding_v2 <=> $1
          LIMIT $3",
    )
    .bind(&qvec)
    .bind(topic_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("scoped search: {e}"), None))?;

    let results: Vec<_> = rows
        .iter()
        .map(|(id, path, sl, el, snippet, sim)| {
            json!({"chunk_id": id, "path": path, "start_line": sl, "end_line": el,
                   "snippet": snippet, "similarity": sim})
        })
        .collect();
    json_result(&json!({
        "topic_id": topic_id,
        "query": params.query,
        "count": results.len(),
        "results": results,
        "guidance": if results.is_empty() {
            Some("no chunk→topic assignments for this topic — run trigger_cron job=\"topic-clustering\"")
        } else { None },
    }))
}

/// #11 — forecast the architecture-quality trajectory (the dimension topic
/// cohesion feeds) from `quality_report_history`, with an ETA to a threshold.
pub async fn tool_topic_quality_forecast(
    ctx: &SystemContext,
    params: TopicQualityForecastParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let threshold = params.threshold.unwrap_or(0.6);
    let project_id: Option<i32> = match params.project.as_deref() {
        Some(p) if !p.trim().is_empty() => Some(project_id_or_err(ctx, p).await?),
        _ => None,
    };

    let rows = sqlx::query_as::<_, (DateTime<Utc>, Option<f32>, Option<f32>)>(
        "SELECT computed_at, architecture_gpa, overall_gpa
           FROM quality_report_history
          WHERE ($1::int IS NULL OR project_id = $1)
          ORDER BY computed_at ASC
          LIMIT 1000",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("history: {e}"), None))?;

    if rows.len() < 2 {
        return json_result(&json!({
            "scope": params.project,
            "points": rows.len(),
            "guidance": "need ≥2 quality-history points — the quality-history cron populates quality_report_history over time",
        }));
    }
    let t0 = rows[0].0;
    let points: Vec<(f64, f64)> = rows
        .iter()
        .filter_map(|(t, arch, _)| {
            arch.map(|g| ((*t - t0).num_seconds() as f64 / 86_400.0, g as f64))
        })
        .collect();
    if points.len() < 2 {
        return json_result(&json!({
            "scope": params.project, "points": points.len(),
            "guidance": "need ≥2 points with architecture_gpa recorded",
        }));
    }
    let slope_per_day = forecast::ols_slope(&points);
    let latest = points.last().map(|p| p.1).unwrap_or(0.0);
    let days_to_threshold = slope_per_day
        .and_then(|s| forecast::weeks_to_threshold(latest, s, threshold).map(|w| w * 7.0));

    json_result(&json!({
        "scope": params.project,
        "dimension": "architecture_gpa (topic cohesion contributes to this)",
        "points": points.len(),
        "latest": latest,
        "threshold": threshold,
        "slope_per_day": slope_per_day,
        "days_to_threshold": days_to_threshold,
        "trend": match slope_per_day {
            Some(s) if s > 0.0005 => "improving",
            Some(s) if s < -0.0005 => "declining",
            Some(_) => "flat",
            None => "unknown",
        },
    }))
}

/// #9 — doc vs code topic alignment via Jensen-Shannon divergence over the
/// per-topic doc-chunk / code-chunk distributions. JSD near 0 = docs and code
/// cover the same themes (well-aligned); near 1 = disjoint (documentation drift
/// / undocumented code areas). Per-topic, flags `code_only_undocumented` and
/// `doc_only` themes.
pub async fn tool_doc_code_topic_alignment(
    ctx: &SystemContext,
    params: DocCodeTopicAlignmentParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(100).clamp(1, 1000);
    let rows = sqlx::query_as::<_, (i32, String, i64, i64)>(
        "SELECT t.id, t.label,
                COUNT(*) FILTER (WHERE f.language IN ('markdown','text','rst','org','asciidoc')) AS doc_n,
                COUNT(*) FILTER (WHERE f.language NOT IN ('markdown','text','rst','org','asciidoc')) AS code_n
           FROM chunk_topic_assignments cta
           JOIN file_chunks fc ON fc.id = cta.chunk_id
           JOIN indexed_files f ON f.id = fc.file_id
           JOIN code_topics t ON t.id = cta.topic_id
          WHERE t.scope = 'global'
          GROUP BY t.id, t.label
          ORDER BY (COUNT(*)) DESC
          LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("alignment: {e}"), None))?;

    let doc_total: f64 = rows.iter().map(|r| r.2 as f64).sum();
    let code_total: f64 = rows.iter().map(|r| r.3 as f64).sum();
    // Jensen-Shannon divergence (log2 → [0,1]) over the topic distributions.
    let mut jsd = 0.0f64;
    if doc_total > 0.0 && code_total > 0.0 {
        for r in &rows {
            let p = r.2 as f64 / doc_total;
            let q = r.3 as f64 / code_total;
            let m = 0.5 * (p + q);
            if p > 0.0 {
                jsd += 0.5 * p * (p / m).log2();
            }
            if q > 0.0 {
                jsd += 0.5 * q * (q / m).log2();
            }
        }
    }
    let topics: Vec<_> = rows
        .iter()
        .map(|(id, label, d, c)| {
            let alignment = if *d == 0 && *c > 0 {
                "code_only_undocumented"
            } else if *c == 0 && *d > 0 {
                "doc_only"
            } else {
                "both"
            };
            json!({"topic_id": id, "label": label, "doc_chunks": d, "code_chunks": c, "alignment": alignment})
        })
        .collect();

    json_result(&json!({
        "jensen_shannon_divergence": jsd.clamp(0.0, 1.0),
        "doc_chunks_total": doc_total,
        "code_chunks_total": code_total,
        "topics": topics,
        "note": "JSD near 0 = docs and code cover the same topics; near 1 = disjoint (documentation \
    drift). `code_only_undocumented` topics are code areas with no doc chunks.",
        "guidance": if rows.is_empty() {
            Some("no chunk→topic assignments — run trigger_cron job=\"topic-clustering\"")
        } else { None },
    }))
}

/// #7 — topic ⊗ experiment map: which experiments are anchored to each topic
/// (via experiment_code_anchor.topic_id). Connects the topic model to the
/// experiment ledger (which themes are under active investigation).
pub async fn tool_topic_experiment_map(
    ctx: &SystemContext,
    params: TopicExperimentMapParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(100).clamp(1, 1000);
    let rows = sqlx::query_as::<_, (i32, String, i64, Vec<String>)>(
        "SELECT t.id, t.label, COUNT(DISTINCT eca.experiment_id),
                ARRAY_AGG(DISTINCT e.title)
           FROM experiment_code_anchor eca
           JOIN code_topics t ON t.id = eca.topic_id
           JOIN experiments e ON e.id = eca.experiment_id
          WHERE eca.topic_id IS NOT NULL
          GROUP BY t.id, t.label
          ORDER BY COUNT(DISTINCT eca.experiment_id) DESC
          LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("topic⊗experiment: {e}"), None))?;

    json_result(&json!({
        "count": rows.len(),
        "topics": rows.iter().map(|(id, label, n, titles)| json!({
            "topic_id": id, "label": label, "experiment_count": n, "experiments": titles,
        })).collect::<Vec<_>>(),
        "note": "Topics with anchored experiments (experiment_code_anchor.topic_id) — which themes are \
    under active investigation.",
        "guidance": if rows.is_empty() {
            Some("no topic-anchored experiments — anchor experiments to topics via experiment_anchor_code")
        } else { None },
    }))
}
