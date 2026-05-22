//! `DbClient` trait — the testability seam over `crate::db::queries::*`.
//!
//! ## Why this trait exists
//!
//! The original `crate::db::queries` module exposes 65+ free functions, each
//! taking `pool: &PgPool` as its first argument. Calling code passes
//! `&PgPool` through 100+ sites; no test can intercept those calls without
//! spinning up real Postgres (testcontainers). The `DbClient` trait is the
//! seam where callers obtain query behaviour through `&dyn DbClient` instead
//! of `&PgPool`, so unit tests can swap in a `MockDbClient` from
//! `pgmcp-testing`.
//!
//! ## Design
//!
//! - One method per query function. Method bodies forward to the existing
//!   `crate::db::queries::*` free function unchanged. This keeps Phase 1
//!   diff-friendly (the SQL stays exactly where it was) and lets later
//!   phases delete the free functions once all callers migrate.
//! - `#[async_trait]` to keep the trait object-safe with async methods.
//!   `async_trait` is already a transitive dependency via `sqlx`, so no new
//!   crate is added.
//! - Returns `Result<T, sqlx::Error>` to preserve the existing error
//!   surface — every caller that does `pool.query(...).await?` continues to
//!   work after the rewrite.
//! - The trait is intentionally not split into smaller sub-traits. Most
//!   callers (e.g. `McpServer`) need a wide cross-section of methods; one
//!   `Arc<dyn DbClient>` field per consumer is simpler than juggling
//!   multiple narrower traits.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::queries::{
    self, ChunkEmbeddingRow, ChunkPairSimilarity, ChunkTopicDetailRow, CommitSearchResult,
    CoupledFilePair, DocCoverageRow, DuplicateFilePair, FileComplexityRow, FileContent, FileInfo,
    FileReference, FileSimilarityPair, FileTopicDistributionRow, FileTopicRow, GrepResult,
    IndexedFileMeta, LanguageCount, OrphanChunkRow, OrphanFileSummary, ProjectInfo, SearchResult,
    SimilarityNeighborRow, TextSearchResult, TopicCentroidRow, TopicCoverageRow,
};
use crate::cron::topic_clustering::TopicResult;

/// Database access trait covering every persistent-store operation in
/// `crate::db::queries`. Production implementation: `impl DbClient for PgPool`.
/// Test implementation: `pgmcp_testing::mocks::MockDbClient` (Phase 2).
///
/// The trait is `Send + Sync` so it can be wrapped as `Arc<dyn DbClient>`
/// and shared across async tasks.
#[async_trait]
pub trait DbClient: Send + Sync {
    /// Escape hatch for callers that need a raw `&PgPool` to run inline SQL
    /// not yet expressed as a trait method (e.g. `sqlx::query(...).fetch_all(pool)`
    /// inside `McpServer` tool methods that have not been migrated to the
    /// `crate::db::queries` module).
    ///
    /// Production impl returns `Some(self)`. `MockDbClient` returns `None`,
    /// signalling to inline-SQL callers that they cannot proceed against a
    /// mock backend (those tools are not unit-testable through the trait
    /// alone — they require an integration test against real Postgres).
    ///
    /// Future work: as inline SQL is incrementally moved into
    /// `crate::db::queries` and exposed as proper trait methods, this
    /// escape hatch will see fewer callers and can eventually be removed.
    fn pool(&self) -> Option<&PgPool> {
        None
    }

    // -- projects -------------------------------------------------------------
    async fn upsert_project(
        &self,
        workspace_path: &str,
        path: &str,
        name: &str,
        git_common_dir: Option<&str>,
        git_root_commits: Option<&str>,
    ) -> Result<i32, sqlx::Error>;
    async fn list_projects(&self) -> Result<Vec<ProjectInfo>, sqlx::Error>;
    async fn find_project_by_cwd(&self, cwd: &str) -> Result<Option<ProjectInfo>, sqlx::Error>;
    async fn language_summary(&self, project_name: &str)
    -> Result<Vec<LanguageCount>, sqlx::Error>;
    async fn update_project_scanned(&self, project_id: i32) -> Result<(), sqlx::Error>;
    /// Bulk-bump `last_scanned_at` for every project whose
    /// `workspace_path` matches the given string. Called by the scanner
    /// / rescan paths after a full walk completes — catches the
    /// "scan happened, nothing changed" case that the per-file
    /// `upsert_project` path misses.
    async fn update_projects_scanned_by_workspace(
        &self,
        workspace_path: &str,
    ) -> Result<u64, sqlx::Error>;
    async fn delete_projects_by_workspace(&self, workspace_path: &str) -> Result<u64, sqlx::Error>;
    async fn cleanup_orphaned_projects(&self) -> Result<u64, sqlx::Error>;
    async fn list_project_names(&self) -> Result<Vec<String>, sqlx::Error>;

    // -- file metadata + chunks ----------------------------------------------
    async fn get_all_file_metadata(&self) -> Result<Vec<IndexedFileMeta>, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn upsert_file(
        &self,
        project_id: i32,
        path: &str,
        relative_path: &str,
        language: &str,
        size_bytes: i64,
        content: Option<&str>,
        content_hash: Option<i64>,
        line_count: i32,
        truncated: bool,
        content_recoverable_from_disk: bool,
        modified_at: DateTime<Utc>,
    ) -> Result<i64, sqlx::Error>;
    async fn get_content_hash(&self, path: &str) -> Result<Option<i64>, sqlx::Error>;
    async fn finalize_file_hash(&self, file_id: i64, content_hash: i64) -> Result<(), sqlx::Error>;
    async fn delete_file_chunks(&self, file_id: i64) -> Result<(), sqlx::Error>;
    async fn insert_chunk(
        &self,
        file_id: i64,
        chunk_index: i32,
        content: &str,
        start_line: i32,
        end_line: i32,
        embedding: &[f32],
    ) -> Result<(), sqlx::Error>;
    /// Batched chunk insert. Wraps N inserts in one transaction so the
    /// embed-pool worker holds one pooled connection across the whole
    /// batch instead of N. Returns a `ChunkBatchOutcome` capturing the
    /// all-or-nothing result. See `crate::db::queries::insert_chunks_batch`.
    async fn insert_chunks_batch(
        &self,
        file_id: i64,
        chunks: &[queries::ChunkInsert<'_>],
    ) -> Result<queries::ChunkBatchOutcome, sqlx::Error>;
    async fn delete_file(&self, path: &str) -> Result<(), sqlx::Error>;
    async fn delete_files_batch(&self, paths: &[String]) -> Result<u64, sqlx::Error>;
    async fn get_file_id_by_path(&self, path: &str) -> Result<Option<i64>, sqlx::Error>;
    async fn get_file_line_count(&self, file_id: i64) -> Result<i32, sqlx::Error>;
    async fn cleanup_stale_files(&self) -> Result<u64, sqlx::Error>;

    // -- search --------------------------------------------------------------
    async fn semantic_search(
        &self,
        embedding: &[f32],
        limit: i32,
        language: Option<&str>,
        project: Option<&str>,
        ef_search: i32,
        dedupe_worktrees: bool,
    ) -> Result<Vec<SearchResult>, sqlx::Error>;
    async fn text_search(
        &self,
        query: &str,
        limit: i32,
        language: Option<&str>,
        dedupe_worktrees: bool,
    ) -> Result<Vec<TextSearchResult>, sqlx::Error>;
    async fn grep_search(
        &self,
        pattern: &str,
        glob: Option<&str>,
        limit: i32,
        dedupe_worktrees: bool,
    ) -> Result<Vec<GrepResult>, sqlx::Error>;
    async fn read_file(&self, path: &str) -> Result<Option<FileContent>, sqlx::Error>;
    async fn read_file_by_relative_path(
        &self,
        relative_path: &str,
    ) -> Result<Option<FileContent>, sqlx::Error>;
    async fn file_info(&self, path: &str) -> Result<Option<FileInfo>, sqlx::Error>;
    async fn file_chunk_summary(
        &self,
        path: &str,
    ) -> Result<crate::db::queries::FileChunkSummary, sqlx::Error>;
    async fn get_file_region_by_lines(
        &self,
        path: &str,
        start_line: i32,
        end_line: i32,
    ) -> Result<Vec<crate::db::queries::FileChunkRow>, sqlx::Error>;
    async fn get_chunks_in_index_range(
        &self,
        path: &str,
        idx_start: i32,
        idx_end: i32,
    ) -> Result<Vec<crate::db::queries::FileChunkRow>, sqlx::Error>;
    async fn grep_search_chunks(
        &self,
        pattern: &str,
        project: Option<&str>,
        language: Option<&str>,
        glob: Option<&str>,
        case_insensitive: bool,
        limit: i32,
        dedupe_worktrees: bool,
    ) -> Result<Vec<crate::db::queries::GrepChunkResult>, sqlx::Error>;
    async fn find_canonical_by_content_hash(
        &self,
        project_id: i32,
        content_hash: i64,
    ) -> Result<Option<crate::db::queries::CanonicalFileMatch>, sqlx::Error>;
    async fn update_file_path_in_place(
        &self,
        file_id: i64,
        new_path: &str,
        new_relative_path: &str,
        modified_at: DateTime<Utc>,
    ) -> Result<(), sqlx::Error>;
    async fn insert_duplicate_file(
        &self,
        project_id: i32,
        path: &str,
        relative_path: &str,
        language: &str,
        size_bytes: i64,
        content_hash: i64,
        canonical_file_id: i64,
        modified_at: DateTime<Utc>,
    ) -> Result<i64, sqlx::Error>;
    async fn project_tree(
        &self,
        project_name: &str,
        depth: i32,
    ) -> Result<Vec<String>, sqlx::Error>;
    async fn search_file_paths(&self, prefix: &str, limit: i32)
    -> Result<Vec<String>, sqlx::Error>;
    async fn list_languages(&self) -> Result<Vec<String>, sqlx::Error>;
    async fn resolve_file_reference(
        &self,
        file_ref: &str,
    ) -> Result<Option<FileReference>, sqlx::Error>;
    async fn find_files_by_path_pattern(
        &self,
        project: &str,
        pattern: &str,
    ) -> Result<Vec<FileReference>, sqlx::Error>;

    // -- statistics ----------------------------------------------------------
    async fn count_indexed_files(&self) -> Result<u64, sqlx::Error>;
    async fn count_chunks(&self) -> Result<u64, sqlx::Error>;
    async fn count_projects(&self) -> Result<u64, sqlx::Error>;
    async fn total_bytes_indexed(&self) -> Result<u64, sqlx::Error>;
    async fn max_chunk_id(&self) -> Result<i64, sqlx::Error>;

    // -- git history ---------------------------------------------------------
    async fn upsert_git_commit(
        &self,
        project_id: i32,
        commit_hash: &str,
        author: &str,
        author_date: DateTime<Utc>,
        subject: &str,
        body: Option<&str>,
    ) -> Result<i64, sqlx::Error>;
    async fn insert_git_commit_chunk(
        &self,
        commit_id: i64,
        chunk_index: i32,
        content: &str,
        embedding: &[f32],
    ) -> Result<(), sqlx::Error>;
    async fn get_git_last_commit(&self, project_id: i32) -> Result<Option<String>, sqlx::Error>;
    async fn set_git_last_commit(&self, project_id: i32, sha: &str) -> Result<(), sqlx::Error>;
    async fn update_blame_for_file(
        &self,
        file_id: i64,
        blame_commit: &str,
        blame_author: &str,
        blame_date: DateTime<Utc>,
        start_line: i32,
        end_line: i32,
    ) -> Result<(), sqlx::Error>;
    async fn semantic_search_commits(
        &self,
        embedding: &[f32],
        limit: i32,
        project: Option<&str>,
        ef_search: i32,
    ) -> Result<Vec<CommitSearchResult>, sqlx::Error>;
    async fn get_git_enabled_projects(&self) -> Result<Vec<(i32, String)>, sqlx::Error>;
    async fn insert_commit_file(
        &self,
        commit_id: i64,
        file_path: &str,
        change_type: char,
    ) -> Result<(), sqlx::Error>;
    async fn get_commits_missing_files(
        &self,
        project_id: i32,
    ) -> Result<Vec<(i64, String)>, sqlx::Error>;
    async fn has_commit_files_for_project(&self, project: &str) -> Result<bool, sqlx::Error>;

    // -- cross-project similarity --------------------------------------------
    async fn compare_two_files(
        &self,
        file_id_a: i64,
        file_id_b: i64,
        ef_search: i32,
    ) -> Result<Vec<ChunkPairSimilarity>, sqlx::Error>;
    async fn batch_find_cross_project_neighbors(
        &self,
        last_chunk_id: i64,
        batch_size: i32,
        top_k: i32,
        threshold: f64,
        ef_search: i32,
    ) -> Result<Vec<SimilarityNeighborRow>, sqlx::Error>;
    async fn insert_similarity_pairs(
        &self,
        rows: &[SimilarityNeighborRow],
    ) -> Result<u64, sqlx::Error>;
    async fn clear_similarity_table(&self) -> Result<(), sqlx::Error>;
    async fn count_similarity_pairs(&self) -> Result<u64, sqlx::Error>;
    async fn top_similar_file_pairs(
        &self,
        limit: i32,
    ) -> Result<Vec<FileSimilarityPair>, sqlx::Error>;
    async fn find_similar_files(
        &self,
        file_id: i64,
        min_similarity: f64,
        limit: i32,
        target_project: Option<&str>,
        include_same_repo: bool,
    ) -> Result<Vec<FileSimilarityPair>, sqlx::Error>;
    async fn find_duplicate_file_pairs(
        &self,
        min_similarity: f64,
        language: Option<&str>,
        limit: i32,
        include_same_repo: bool,
    ) -> Result<Vec<DuplicateFilePair>, sqlx::Error>;

    // -- topic clustering ----------------------------------------------------
    async fn bulk_extract_embeddings(
        &self,
        language: Option<&str>,
    ) -> Result<Vec<ChunkEmbeddingRow>, sqlx::Error>;
    async fn bulk_extract_project_embeddings(
        &self,
        project_name: &str,
        language: Option<&str>,
    ) -> Result<Vec<ChunkEmbeddingRow>, sqlx::Error>;
    async fn clear_topics_for_scope(&self, scope: &str) -> Result<(), sqlx::Error>;
    async fn store_topics(&self, scope: &str, topics: &[TopicResult]) -> Result<(), sqlx::Error>;
    async fn load_cached_topics(
        &self,
        scope: &str,
        limit: i32,
    ) -> Result<Vec<serde_json::Value>, sqlx::Error>;
    async fn has_topic_assignments(&self) -> Result<bool, sqlx::Error>;
    async fn load_topic_centroids(&self, scope: &str)
    -> Result<Vec<TopicCentroidRow>, sqlx::Error>;

    // -- analysis tools ------------------------------------------------------
    async fn find_orphan_chunks(
        &self,
        project: Option<&str>,
        language: Option<&str>,
        limit: i32,
    ) -> Result<Vec<OrphanChunkRow>, sqlx::Error>;
    async fn find_orphan_file_summary(
        &self,
        project: Option<&str>,
    ) -> Result<Vec<OrphanFileSummary>, sqlx::Error>;
    async fn load_chunk_topic_assignments_for_files(
        &self,
        project: Option<&str>,
    ) -> Result<Vec<FileTopicRow>, sqlx::Error>;
    async fn find_coupled_files(
        &self,
        project: &str,
        min_coupling: f64,
        min_commits: i32,
    ) -> Result<Vec<CoupledFilePair>, sqlx::Error>;
    async fn get_file_complexity_data(
        &self,
        project: &str,
    ) -> Result<Vec<FileComplexityRow>, sqlx::Error>;
    async fn get_test_topic_coverage(
        &self,
        project: &str,
    ) -> Result<Vec<TopicCoverageRow>, sqlx::Error>;
    async fn get_file_topic_distributions(
        &self,
        project: &str,
        language: Option<&str>,
    ) -> Result<Vec<FileTopicDistributionRow>, sqlx::Error>;
    async fn get_chunk_topic_details(
        &self,
        project: &str,
        language: Option<&str>,
    ) -> Result<Vec<ChunkTopicDetailRow>, sqlx::Error>;
    async fn get_doc_topic_coverage(
        &self,
        project: &str,
    ) -> Result<Vec<DocCoverageRow>, sqlx::Error>;
}

// ============================================================================
// Production impl: forwards to the existing `crate::db::queries::*` free fns.
// ============================================================================

mod pg_impl;
