//! Semantic table discovery: rank data tables by cosine similarity of their
//! name+description embedding to a natural-language query.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

use super::{db_err, invalid, resolve_scope};
use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::DataTableSearchParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_data_table_search(
    ctx: &SystemContext,
    params: DataTableSearchParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let query = params.query.trim();
    if query.is_empty() {
        return Err(invalid("`query` must be non-empty"));
    }
    let scope = resolve_scope(pool, params.project.as_deref()).await?;
    let limit = params.limit.unwrap_or(10).clamp(1, 100);

    let embedding = ctx
        .embed()
        .embed_query(query)
        .await
        .map_err(|e| McpError::internal_error(format!("embed query: {e}"), None))?;

    let hits = queries::search_tables(pool, &embedding, scope, limit)
        .await
        .map_err(db_err)?;

    // Tables are embedded asynchronously (on write, with cron backfill), so a
    // freshly created table may not be searchable until its embedding lands.
    let results: Vec<Value> = hits
        .iter()
        .map(|(t, sim)| {
            let mut v = serde_json::to_value(t).unwrap_or(Value::Null);
            if let Value::Object(ref mut m) = v {
                m.insert("similarity".into(), json!(sim));
            }
            v
        })
        .collect();

    json_result(&json!({ "query": query, "count": results.len(), "results": results }))
}
