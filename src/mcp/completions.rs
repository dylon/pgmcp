//! Completion handler for MCP `completion/complete` requests.
//!
//! Supports completing parameters in resource templates:
//! - `{name}` → project names
//! - `{path}` → file relative paths

use rmcp::ErrorData as McpError;
use rmcp::model::*;
use sqlx::PgPool;

/// Handle a completion request by matching the reference and argument.
pub async fn handle_complete(
    db_pool: &PgPool,
    request: CompleteRequestParams,
) -> Result<CompleteResult, McpError> {
    let values = match &request.r#ref {
        Reference::Resource(resource_ref) => {
            let uri = &resource_ref.uri;
            let arg_name = &request.argument.name;
            let prefix = &request.argument.value;

            if uri.contains("{name}") && arg_name == "name" {
                complete_project_names(db_pool, prefix).await?
            } else if uri.contains("{path}") && arg_name == "path" {
                complete_file_paths(db_pool, prefix).await?
            } else {
                Vec::new()
            }
        }
        Reference::Prompt(_) => {
            // pgmcp doesn't use prompts
            Vec::new()
        }
    };

    let completion = CompletionInfo::new(values)
        .map_err(|e| McpError::internal_error(format!("Completion error: {}", e), None))?;

    Ok(CompleteResult::new(completion))
}

async fn complete_project_names(db_pool: &PgPool, prefix: &str) -> Result<Vec<String>, McpError> {
    let names = crate::db::queries::list_project_names(db_pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

    let filtered: Vec<String> = names
        .into_iter()
        .filter(|n| n.starts_with(prefix))
        .take(CompletionInfo::MAX_VALUES)
        .collect();

    Ok(filtered)
}

async fn complete_file_paths(db_pool: &PgPool, prefix: &str) -> Result<Vec<String>, McpError> {
    let paths =
        crate::db::queries::search_file_paths(db_pool, prefix, CompletionInfo::MAX_VALUES as i32)
            .await
            .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

    Ok(paths)
}
