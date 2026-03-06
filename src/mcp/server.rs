//! MCP Server implementation using rmcp.

use std::sync::Arc;

use arc_swap::ArcSwap;
use rmcp::model::*;
use rmcp::schemars;
use rmcp::{tool, tool_router, tool_handler};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ServerHandler, ErrorData as McpError, RoleServer};
use rmcp::service::RequestContext;
use serde::Deserialize;
use sqlx::PgPool;

use crate::config::Config;
use crate::stats::tracker::StatsTracker;

/// MCP Server state.
#[derive(Clone)]
pub struct McpServer {
    db_pool: PgPool,
    embed_model: Arc<tokio::sync::Mutex<fastembed::TextEmbedding>>,
    stats: Arc<StatsTracker>,
    #[allow(dead_code)]
    config: Arc<ArcSwap<Config>>,
    tool_router: ToolRouter<McpServer>,
}

// === Tool parameter types ===

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SemanticSearchParams {
    #[schemars(description = "Search query text")]
    pub query: String,
    #[schemars(description = "Maximum number of results (default: 10)")]
    pub limit: Option<i32>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TextSearchParams {
    #[schemars(description = "Full-text search query")]
    pub query: String,
    #[schemars(description = "Maximum number of results (default: 10)")]
    pub limit: Option<i32>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GrepParams {
    #[schemars(description = "Regex pattern to search for")]
    pub pattern: String,
    #[schemars(description = "Glob pattern to filter files (e.g. '*.rs')")]
    pub glob: Option<String>,
    #[schemars(description = "Maximum number of results (default: 10)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadFileParams {
    #[schemars(description = "Absolute path of the file to read")]
    pub path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectTreeParams {
    #[schemars(description = "Project name")]
    pub project: String,
    #[schemars(description = "Maximum directory depth (default: 5)")]
    pub depth: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FileInfoParams {
    #[schemars(description = "Absolute path of the file")]
    pub path: String,
}

#[tool_router]
impl McpServer {
    pub fn new(
        db_pool: PgPool,
        embed_model: Arc<tokio::sync::Mutex<fastembed::TextEmbedding>>,
        stats: Arc<StatsTracker>,
        config: Arc<ArcSwap<Config>>,
    ) -> Self {
        Self {
            db_pool,
            embed_model,
            stats,
            config,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Search indexed code using semantic similarity (vector embeddings). Best for conceptual queries like 'error handling' or 'database connection setup'.")]
    async fn semantic_search(
        &self,
        Parameters(params): Parameters<SemanticSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats.mcp_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.stats.semantic_searches.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let limit = params.limit.unwrap_or(10);

        // Embed the query
        let embedding = {
            let model = self.embed_model.lock().await;
            model.embed(vec![&params.query], None)
                .map_err(|e| McpError::internal_error(format!("Embedding failed: {}", e), None))?
                .into_iter()
                .next()
                .ok_or_else(|| McpError::internal_error("No embedding returned", None))?
        };

        let results = crate::db::queries::semantic_search(
            &self.db_pool,
            &embedding,
            limit,
            params.language.as_deref(),
        )
        .await
        .map_err(|e| McpError::internal_error(format!("Search failed: {}", e), None))?;

        let json = serde_json::to_string_pretty(&results)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(description = "Search indexed code using PostgreSQL full-text search. Best for exact keyword matches.")]
    async fn text_search(
        &self,
        Parameters(params): Parameters<TextSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats.mcp_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.stats.text_searches.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let limit = params.limit.unwrap_or(10);

        let results = crate::db::queries::text_search(
            &self.db_pool,
            &params.query,
            limit,
            params.language.as_deref(),
        )
        .await
        .map_err(|e| McpError::internal_error(format!("Search failed: {}", e), None))?;

        let json = serde_json::to_string_pretty(&results)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(description = "Search indexed files using a regex pattern across file contents.")]
    async fn grep(
        &self,
        Parameters(params): Parameters<GrepParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats.mcp_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.stats.grep_searches.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let limit = params.limit.unwrap_or(10);

        let results = crate::db::queries::grep_search(
            &self.db_pool,
            &params.pattern,
            params.glob.as_deref(),
            limit,
        )
        .await
        .map_err(|e| McpError::internal_error(format!("Grep failed: {}", e), None))?;

        let json = serde_json::to_string_pretty(&results)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(description = "Read the content of an indexed file by its absolute path.")]
    async fn read_file(
        &self,
        Parameters(params): Parameters<ReadFileParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats.mcp_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let result = crate::db::queries::read_file(&self.db_pool, &params.path)
            .await
            .map_err(|e| McpError::internal_error(format!("Read failed: {}", e), None))?;

        match result {
            Some(file) => {
                let json = serde_json::to_string_pretty(&file)
                    .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            None => Ok(CallToolResult::success(vec![Content::text(format!(
                "File not found in index: {}",
                params.path
            ))])),
        }
    }

    #[tool(description = "List all discovered projects with file counts.")]
    async fn list_projects(&self) -> Result<CallToolResult, McpError> {
        self.stats.mcp_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let projects = crate::db::queries::list_projects(&self.db_pool)
            .await
            .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

        let json = serde_json::to_string_pretty(&projects)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(description = "Show the file tree for a project, limited by depth.")]
    async fn project_tree(
        &self,
        Parameters(params): Parameters<ProjectTreeParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats.mcp_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let depth = params.depth.unwrap_or(5);

        let paths = crate::db::queries::project_tree(&self.db_pool, &params.project, depth)
            .await
            .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

        let tree = paths.join("\n");
        Ok(CallToolResult::success(vec![Content::text(tree)]))
    }

    #[tool(description = "Get metadata about an indexed file (size, language, line count, last indexed).")]
    async fn file_info(
        &self,
        Parameters(params): Parameters<FileInfoParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats.mcp_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let info = crate::db::queries::file_info(&self.db_pool, &params.path)
            .await
            .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

        match info {
            Some(info) => {
                let json = serde_json::to_string_pretty(&info)
                    .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            None => Ok(CallToolResult::success(vec![Content::text(format!(
                "File not found in index: {}",
                params.path
            ))])),
        }
    }

    #[tool(description = "Get overall indexing statistics including file counts, search counts, and pool state.")]
    async fn index_stats(&self) -> Result<CallToolResult, McpError> {
        self.stats.mcp_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let snapshot = self.stats.snapshot();
        let json = serde_json::to_string_pretty(&snapshot)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

#[tool_handler]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(
            Implementation::new("pgmcp", env!("CARGO_PKG_VERSION")),
        )
        .with_instructions(
            "pgmcp indexes configured file types from workspaces into PostgreSQL with pgvector embeddings. \
             Use semantic_search for conceptual queries, text_search for keyword matches, \
             grep for regex patterns. list_projects shows indexed projects.",
        )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult {
            resources: vec![
                RawResource::new("pgmcp://stats", "Indexing Statistics")
                    .with_description("Current indexing statistics (JSON)")
                    .no_annotation(),
                RawResource::new("pgmcp://projects", "Indexed Projects")
                    .with_description("List of indexed projects (JSON)")
                    .no_annotation(),
            ],
            next_cursor: None,
            meta: None,
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        match request.uri.as_str() {
            "pgmcp://stats" => {
                let snapshot = self.stats.snapshot();
                let json = serde_json::to_string_pretty(&snapshot)
                    .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                Ok(ReadResourceResult::new(vec![
                    ResourceContents::text(json, request.uri.clone()),
                ]))
            }
            "pgmcp://projects" => {
                let projects = crate::db::queries::list_projects(&self.db_pool)
                    .await
                    .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                let json = serde_json::to_string_pretty(&projects)
                    .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                Ok(ReadResourceResult::new(vec![
                    ResourceContents::text(json, request.uri.clone()),
                ]))
            }
            _ => Err(McpError::resource_not_found(
                format!("Unknown resource: {}", request.uri),
                None,
            )),
        }
    }
}
