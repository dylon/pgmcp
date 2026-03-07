//! MCP Server implementation using rmcp.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use arc_swap::ArcSwap;
use rmcp::model::*;
use rmcp::schemars;
use rmcp::{tool, tool_router, tool_handler};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ServerHandler, ErrorData as McpError, RoleServer};
use rmcp::service::{RequestContext, NotificationContext};
use serde::Deserialize;
use sqlx::PgPool;

use crate::config::Config;
use crate::stats::tracker::StatsTracker;

use super::logging::LogBroadcaster;
use super::tasks::TaskStore;

/// MCP Server state.
#[derive(Clone)]
pub struct McpServer {
    db_pool: PgPool,
    embed_model: Arc<tokio::sync::Mutex<fastembed::TextEmbedding>>,
    stats: Arc<StatsTracker>,
    #[allow(dead_code)]
    config: Arc<ArcSwap<Config>>,
    tool_router: ToolRouter<McpServer>,
    log_broadcaster: Arc<LogBroadcaster>,
    task_store: Arc<TaskStore>,
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
    #[schemars(description = "Filter by project name")]
    pub project: Option<String>,
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
        log_broadcaster: Arc<LogBroadcaster>,
        task_store: Arc<TaskStore>,
    ) -> Self {
        Self {
            db_pool,
            embed_model,
            stats,
            config,
            tool_router: Self::tool_router(),
            log_broadcaster,
            task_store,
        }
    }

    #[tool(description = "Search indexed code using semantic similarity (vector embeddings). Best for conceptual queries like 'error handling' or 'database connection setup'.")]
    async fn semantic_search(
        &self,
        Parameters(params): Parameters<SemanticSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.semantic_searches.fetch_add(1, Ordering::Relaxed);

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

        let ef_search = self.config.load().vector.ef_search;
        let results = crate::db::queries::semantic_search(
            &self.db_pool,
            &embedding,
            limit,
            params.language.as_deref(),
            params.project.as_deref(),
            ef_search,
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
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.text_searches.fetch_add(1, Ordering::Relaxed);

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
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.grep_searches.fetch_add(1, Ordering::Relaxed);

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
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);

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
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);

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
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);

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
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);

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
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);

        let snapshot = self.stats.snapshot();
        let json = serde_json::to_string_pretty(&snapshot)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(description = "Trigger a full re-index of all workspaces. Clears the existing index and restarts indexing. Can be invoked as a long-running task.")]
    async fn reindex(&self) -> Result<CallToolResult, McpError> {
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);

        // Synchronous (non-task) reindex: clear index directly
        sqlx::query("DELETE FROM file_chunks")
            .execute(&self.db_pool)
            .await
            .map_err(|e| McpError::internal_error(format!("Failed to clear chunks: {}", e), None))?;

        sqlx::query("DELETE FROM indexed_files")
            .execute(&self.db_pool)
            .await
            .map_err(|e| McpError::internal_error(format!("Failed to clear files: {}", e), None))?;

        self.log_broadcaster.log(
            LoggingLevel::Info,
            "pgmcp::reindex",
            serde_json::json!({"message": "Index cleared via reindex tool"}),
        );

        Ok(CallToolResult::success(vec![Content::text(
            "Index cleared. Files will be re-indexed automatically by the background scanner.",
        )]))
    }
}

#[tool_handler]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_completions()
                .enable_logging()
                .enable_tasks()
                .build(),
        )
        .with_server_info(
            Implementation::new("pgmcp", env!("CARGO_PKG_VERSION")),
        )
        .with_instructions(
            "pgmcp indexes source code from the user's development workspaces into PostgreSQL \
             with pgvector embeddings. It maintains a continuously-updated index of all projects.\n\n\
             WHEN TO USE pgmcp (prefer over built-in Grep/Glob/Read for these cases):\n\
             - Cross-project searches: find patterns, functions, or concepts across ALL indexed projects at once\n\
             - Semantic/conceptual queries: \"error handling patterns\", \"database connection setup\", \
               \"authentication flow\" — use semantic_search (vector similarity)\n\
             - Keyword searches across the full indexed codebase: use text_search (PostgreSQL full-text)\n\
             - Regex searches across all indexed files: use grep\n\
             - Discovering what projects exist and their structure: use list_projects, project_tree\n\
             - Reading indexed files without filesystem access: use read_file\n\
             - Checking indexing health: use index_stats\n\n\
             Built-in tools (Grep/Glob/Read) are better for single-file or single-directory operations \
             in the current working directory. pgmcp is better for broad, cross-project exploration \
             and semantic understanding of the codebase.",
        )
    }

    // ── Lifecycle ────────────────────────────────────────────────────────

    async fn on_initialized(
        &self,
        context: NotificationContext<RoleServer>,
    ) {
        tracing::info!("Client initialized, registering peer for log broadcasting");
        self.log_broadcaster.add_peer(context.peer.clone());
    }

    // ── Completions ──────────────────────────────────────────────────────

    async fn complete(
        &self,
        request: CompleteRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, McpError> {
        super::completions::handle_complete(&self.db_pool, request).await
    }

    // ── Logging ──────────────────────────────────────────────────────────

    async fn set_level(
        &self,
        request: SetLevelRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        tracing::info!(level = ?request.level, "Client set logging level");
        self.log_broadcaster.set_level(request.level);
        Ok(())
    }

    // ── Resources ────────────────────────────────────────────────────────

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

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult {
            resource_templates: vec![
                RawResourceTemplate::new("pgmcp://project/{name}", "Project Info")
                    .with_description("Project details by name")
                    .no_annotation(),
                RawResourceTemplate::new("pgmcp://project/{name}/tree", "Project Tree")
                    .with_description("File tree for a project")
                    .no_annotation(),
                RawResourceTemplate::new("pgmcp://file/{path}", "File Content")
                    .with_description("Read an indexed file by relative path")
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
        let uri: &str = &request.uri;

        // Static resources
        match uri {
            "pgmcp://stats" => {
                let snapshot = self.stats.snapshot();
                let json = serde_json::to_string_pretty(&snapshot)
                    .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                return Ok(ReadResourceResult::new(vec![
                    ResourceContents::text(json, request.uri.clone()),
                ]));
            }
            "pgmcp://projects" => {
                let projects = crate::db::queries::list_projects(&self.db_pool)
                    .await
                    .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                let json = serde_json::to_string_pretty(&projects)
                    .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                return Ok(ReadResourceResult::new(vec![
                    ResourceContents::text(json, request.uri.clone()),
                ]));
            }
            _ => {}
        }

        // Templated resources
        if let Some(rest) = uri.strip_prefix("pgmcp://project/") {
            if let Some(name) = rest.strip_suffix("/tree") {
                // pgmcp://project/{name}/tree
                let paths = crate::db::queries::project_tree(&self.db_pool, name, 10)
                    .await
                    .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                let tree = paths.join("\n");
                return Ok(ReadResourceResult::new(vec![
                    ResourceContents::text(tree, request.uri.clone()),
                ]));
            }
            // pgmcp://project/{name}
            let name = rest;
            let projects = crate::db::queries::list_projects(&self.db_pool)
                .await
                .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
            let project = projects.into_iter().find(|p| p.name == name);
            match project {
                Some(p) => {
                    let json = serde_json::to_string_pretty(&p)
                        .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                    return Ok(ReadResourceResult::new(vec![
                        ResourceContents::text(json, request.uri.clone()),
                    ]));
                }
                None => {
                    return Err(McpError::resource_not_found(
                        format!("Project not found: {}", name),
                        None,
                    ));
                }
            }
        }

        if let Some(path) = uri.strip_prefix("pgmcp://file/") {
            // pgmcp://file/{path} — search by relative_path
            let file = crate::db::queries::read_file_by_relative_path(&self.db_pool, path)
                .await
                .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
            match file {
                Some(f) => {
                    let json = serde_json::to_string_pretty(&f)
                        .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                    return Ok(ReadResourceResult::new(vec![
                        ResourceContents::text(json, request.uri.clone()),
                    ]));
                }
                None => {
                    return Err(McpError::resource_not_found(
                        format!("File not found: {}", path),
                        None,
                    ));
                }
            }
        }

        Err(McpError::resource_not_found(
            format!("Unknown resource: {}", uri),
            None,
        ))
    }

    // ── Tasks ────────────────────────────────────────────────────────────

    async fn enqueue_task(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CreateTaskResult, McpError> {
        match &*request.name {
            "reindex" => {
                let (task_id, cancel_flag) = self.task_store.create_task("reindex");
                let task = self.task_store.get_task(&task_id)
                    .expect("Task was just created");

                let db_pool = self.db_pool.clone();
                let task_store = Arc::clone(&self.task_store);
                let log_broadcaster = Arc::clone(&self.log_broadcaster);

                tokio::spawn(async move {
                    task_store.update_progress(&task_id, "Clearing file chunks...");
                    log_broadcaster.log(
                        LoggingLevel::Info,
                        "pgmcp::reindex",
                        serde_json::json!({"message": "Reindex task started, clearing chunks"}),
                    );

                    if cancel_flag.load(Ordering::Acquire) {
                        return;
                    }

                    if let Err(e) = sqlx::query("DELETE FROM file_chunks")
                        .execute(&db_pool)
                        .await
                    {
                        task_store.fail_task(&task_id, &format!("Failed to clear chunks: {}", e));
                        return;
                    }

                    task_store.update_progress(&task_id, "Clearing indexed files...");

                    if cancel_flag.load(Ordering::Acquire) {
                        return;
                    }

                    if let Err(e) = sqlx::query("DELETE FROM indexed_files")
                        .execute(&db_pool)
                        .await
                    {
                        task_store.fail_task(&task_id, &format!("Failed to clear files: {}", e));
                        return;
                    }

                    log_broadcaster.log(
                        LoggingLevel::Info,
                        "pgmcp::reindex",
                        serde_json::json!({"message": "Index cleared, background scanner will re-index"}),
                    );

                    task_store.complete_task(
                        &task_id,
                        serde_json::json!({
                            "message": "Index cleared. Files will be re-indexed automatically by the background scanner."
                        }),
                    );
                });

                Ok(CreateTaskResult::new(task))
            }
            other => Err(McpError::internal_error(
                format!("Task processing not supported for tool: {}", other),
                None,
            )),
        }
    }

    async fn list_tasks(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListTasksResult, McpError> {
        Ok(ListTasksResult::new(self.task_store.list_tasks()))
    }

    async fn get_task_info(
        &self,
        request: GetTaskInfoParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        match self.task_store.get_task(&request.task_id) {
            Some(task) => Ok(GetTaskResult {
                meta: None,
                task,
            }),
            None => Err(McpError::internal_error(
                format!("Task not found: {}", request.task_id),
                None,
            )),
        }
    }

    async fn get_task_result(
        &self,
        request: GetTaskResultParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskPayloadResult, McpError> {
        match self.task_store.get_result(&request.task_id) {
            Some(result) => Ok(GetTaskPayloadResult::new(result)),
            None => {
                // Check if task exists but has no result yet
                if self.task_store.get_task(&request.task_id).is_some() {
                    Err(McpError::internal_error("Task is still in progress", None))
                } else {
                    Err(McpError::internal_error(
                        format!("Task not found: {}", request.task_id),
                        None,
                    ))
                }
            }
        }
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CancelTaskResult, McpError> {
        match self.task_store.cancel_task(&request.task_id) {
            Some(task) => Ok(CancelTaskResult {
                meta: None,
                task,
            }),
            None => Err(McpError::internal_error(
                format!("Task not found: {}", request.task_id),
                None,
            )),
        }
    }
}
