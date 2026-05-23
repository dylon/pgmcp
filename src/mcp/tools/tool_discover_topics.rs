//! `tool_discover_topics` — MCP tool body, extracted from `super::super::server`.

#![allow(unused_imports)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, LoggingLevel};
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::context::SystemContext;
use crate::mcp::server::*;

pub async fn tool_discover_topics(
    ctx: &SystemContext,
    params: DiscoverTopicsParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().topic_scans.fetch_add(1, Ordering::Relaxed);

    let min_cluster_size = params.min_cluster_size.unwrap_or(5) as usize;
    let limit = params.limit.unwrap_or(30);
    let refresh = params.refresh.unwrap_or(false);

    debug!(
        tool = "discover_topics",
        project = params.project.as_deref().unwrap_or("*"),
        min_cluster_size,
        language = params.language.as_deref().unwrap_or("*"),
        limit,
        refresh,
        "MCP tool invoked",
    );

    if let Some(ref project_name) = params.project {
        // On-demand per-project scan
        let config = ctx.config().load();
        let summary = crate::cron::topic_clustering::run_project_topic_scan(
            ctx.db().pool().expect(
                "inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
            ),
            project_name,
            &config.cron,
            min_cluster_size,
            params.language.as_deref(),
        )
        .await
        .map_err(|e| {
            error!(tool = "discover_topics", error = %e, "MCP tool failed");
            McpError::internal_error(format!("Topic scan failed: {}", e), None)
        })?;

        let result = format_clustering_summary(&summary, limit);
        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "discover_topics",
            topics = summary.topics_found,
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed (project scan)",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    } else {
        // Global: refresh or load cached
        if refresh {
            let config = ctx.config().load();
            let stats = Arc::clone(ctx.stats());
            crate::cron::topic_clustering::run_global_topic_scan(
                ctx.db().pool().expect(
                    "inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
                ),
                &config.cron,
                &stats,
                ctx.lifecycle(),
            )
            .await;
        }

        let cached = ctx
            .db()
            .load_cached_topics("global", limit)
            .await
            .map_err(|e| {
                error!(tool = "discover_topics", error = %e, "MCP tool failed");
                McpError::internal_error(format!("Load cached topics failed: {}", e), None)
            })?;

        // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown
        // for the project. Universal enrichment — every tool benefits from
        // surfacing the effect distribution alongside its primary output.
        // Gracefully degrades to empty when the project lookup or
        // shadow-ASR data isn't populated.
        let effect_breakdown: Vec<serde_json::Value> = (async {
            let Some(pool) = ctx.db().pool() else {
                return Vec::new();
            };
            let project_id_opt: Option<i32> =
                sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
                    .bind(&params.project)
                    .fetch_optional(pool)
                    .await
                    .unwrap_or(None);
            match project_id_opt {
                Some(pid) => crate::mcp::tools::sema_helpers::effects::effect_counts(pool, pid)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
                    .collect(),
                None => Vec::new(),
            }
        })
        .await;

        let result = serde_json::json!({
        "effect_breakdown": effect_breakdown,
            "scope": "global",
            "algorithm": "Fuzzy C-Means + c-TF-IDF",
            "source": if refresh { "freshly computed" } else { "cached" },
            "topics_found": cached.len(),
            "topics": cached,
            "guidance": "Use compare_files to examine specific file pairs within a topic. \
                         Topics with high avg_internal_similarity and multiple files indicate \
                         DRY candidates. Use discover_topics(project: \"name\") for real-time \
                         intra-project analysis. Keywords show c-TF-IDF extracted topic labels.",
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "discover_topics",
            topics = cached.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed (global cached)",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}
