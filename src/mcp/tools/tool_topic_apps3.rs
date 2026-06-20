//! Topic-model applications #3 (drift early-warning) and #8 (ownership
//! forecasting), ADR-029 / item 14.

use std::collections::HashMap;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::{TopicDriftWarningParams, TopicOwnershipForecastParams};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

/// #3 — topic-drift early-warning: per-topic chunk-count change across the
/// `topics_size_history` snapshots (first → last). Large growth/shrink flags an
/// emerging or declining theme — an early signal worth correlating with bug risk.
pub async fn tool_topic_drift_warning(
    ctx: &SystemContext,
    params: TopicDriftWarningParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let min_pct = params.min_pct_change.unwrap_or(0.5);
    let limit = params.limit.unwrap_or(50).clamp(1, 500) as usize;

    let history = queries::get_topics_size_history(pool).await;
    if history.len() < 2 {
        return json_result(&json!({
            "snapshots": history.len(),
            "guidance": "need ≥2 size snapshots — the topics-size-history cron must run at least twice",
        }));
    }

    // First and last observed (label, chunk_count) per topic_id.
    let mut first: HashMap<i64, (String, i64)> = HashMap::new();
    let mut last: HashMap<i64, (String, i64)> = HashMap::new();
    for snap in &history {
        if let Some(topics) = snap.get("topics").and_then(|t| t.as_array()) {
            for t in topics {
                let Some(id) = t.get("topic_id").and_then(|v| v.as_i64()) else {
                    continue;
                };
                let label = t
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let count = t.get("chunk_count").and_then(|v| v.as_i64()).unwrap_or(0);
                first.entry(id).or_insert((label.clone(), count));
                last.insert(id, (label, count));
            }
        }
    }

    let mut drifts: Vec<serde_json::Value> = Vec::new();
    for (id, (label, last_count)) in &last {
        let (_, first_count) = first
            .get(id)
            .cloned()
            .unwrap_or((String::new(), *last_count));
        let delta = last_count - first_count;
        let pct = if first_count > 0 {
            delta as f64 / first_count as f64
        } else if *last_count > 0 {
            1.0
        } else {
            0.0
        };
        if pct.abs() >= min_pct {
            drifts.push(json!({
                "topic_id": id, "label": label,
                "first_chunk_count": first_count, "last_chunk_count": last_count,
                "delta": delta, "pct_change": pct,
                "direction": if delta > 0 { "emerging" } else { "declining" },
            }));
        }
    }
    drifts.sort_by(|a, b| {
        b["pct_change"]
            .as_f64()
            .unwrap_or(0.0)
            .abs()
            .partial_cmp(&a["pct_change"].as_f64().unwrap_or(0.0).abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    drifts.truncate(limit);

    json_result(&json!({
        "snapshots": history.len(),
        "min_pct_change": min_pct,
        "drifting_topic_count": drifts.len(),
        "drifting_topics": drifts,
        "note": "Topics whose chunk count changed by ≥min_pct_change between the first and last \
    size snapshot — emerging (growing) or declining (shrinking) themes. Cross-reference with \
    bug_prediction for files in an emerging topic for early risk warning.",
    }))
}

/// #8 — topic ownership forecasting: per-topic ownership concentration from git
/// blame over the topic's chunks (top-author share + bus factor + Herfindahl),
/// plus a recency trend (recent-180d concentration vs overall) → whether
/// ownership is CONCENTRATING toward a single owner (a future bus-factor risk).
pub async fn tool_topic_ownership_forecast(
    ctx: &SystemContext,
    params: TopicOwnershipForecastParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50).clamp(1, 500) as usize;

    // Per (topic, author): total chunks + recent (180d) chunks.
    let rows = sqlx::query_as::<_, (i32, String, String, i64, i64)>(
        "SELECT t.id, t.label, fc.blame_author,
                COUNT(*) AS total,
                COUNT(*) FILTER (WHERE gc.author_date > NOW() - INTERVAL '180 days') AS recent
           FROM chunk_topic_assignments cta
           JOIN file_chunks fc ON fc.id = cta.chunk_id
           JOIN code_topics t ON t.id = cta.topic_id
           LEFT JOIN git_commits gc ON gc.commit_hash = fc.blame_commit
          WHERE t.scope = 'global' AND fc.blame_author IS NOT NULL AND fc.blame_author <> ''
          GROUP BY t.id, t.label, fc.blame_author",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("ownership: {e}"), None))?;

    // Aggregate per topic.
    struct Topic {
        label: String,
        authors: HashMap<String, (i64, i64)>, // author -> (total, recent)
    }
    let mut topics: HashMap<i32, Topic> = HashMap::new();
    for (id, label, author, total, recent) in rows {
        let t = topics.entry(id).or_insert_with(|| Topic {
            label,
            authors: HashMap::new(),
        });
        let e = t.authors.entry(author).or_insert((0, 0));
        e.0 += total;
        e.1 += recent;
    }

    let mut out: Vec<serde_json::Value> = Vec::new();
    for (id, t) in &topics {
        let total: i64 = t.authors.values().map(|(c, _)| *c).sum();
        let recent_total: i64 = t.authors.values().map(|(_, r)| *r).sum();
        if total == 0 {
            continue;
        }
        let bus_factor = t.authors.len();
        let top_share = t
            .authors
            .values()
            .map(|(c, _)| *c as f64 / total as f64)
            .fold(0.0f64, f64::max);
        // Herfindahl index (Σ share²): 1 = single owner.
        let herfindahl: f64 = t
            .authors
            .values()
            .map(|(c, _)| {
                let s = *c as f64 / total as f64;
                s * s
            })
            .sum();
        // Recent top-share (concentration of the last 180 days).
        let recent_top = if recent_total > 0 {
            t.authors
                .values()
                .map(|(_, r)| *r as f64 / recent_total as f64)
                .fold(0.0f64, f64::max)
        } else {
            top_share
        };
        let trend = if recent_top > top_share + 0.1 {
            "concentrating"
        } else if recent_top < top_share - 0.1 {
            "diffusing"
        } else {
            "stable"
        };
        out.push(json!({
            "topic_id": id, "label": t.label,
            "chunks": total, "bus_factor": bus_factor,
            "top_author_share": top_share, "herfindahl": herfindahl,
            "recent_top_author_share": recent_top, "ownership_trend": trend,
            "single_owner_risk": bus_factor <= 2 || top_share >= 0.8 || (trend == "concentrating" && top_share >= 0.6),
        }));
    }
    // Highest concentration first.
    out.sort_by(|a, b| {
        b["herfindahl"]
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&a["herfindahl"].as_f64().unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(limit);

    json_result(&json!({
        "count": out.len(),
        "topics": out,
        "note": "Per-topic git-blame ownership concentration (top-author share, bus_factor, \
    Herfindahl) + a recency trend (last 180d vs overall). `single_owner_risk` flags topics owned by ≤2 \
    authors, ≥80% by one, or actively concentrating — a forecast of bus-factor risk.",
        "guidance": if out.is_empty() {
            Some("no blame/topic data — run the topic-clustering + git-blame indexing crons")
        } else { None },
    }))
}
