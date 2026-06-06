//! Production `impl DbClient for PgPool`. Extracted from `client.rs` as
//! part of the D.2 god-file split. The trait definition stays in the
//! parent; this file holds only the body that forwards each method to
//! the corresponding free function in `crate::db::queries`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::DbClient;
use crate::cron::topic_clustering::TopicResult;
use crate::db::queries::{
    self, ChunkEmbeddingRow, ChunkPairSimilarity, ChunkTopicDetailRow, CommitSearchResult,
    CoupledFilePair, DocCoverageRow, DuplicateFilePair, FileComplexityRow, FileContent, FileInfo,
    FileReference, FileSimilarityPair, FileTopicDistributionRow, FileTopicRow, GrepResult,
    IndexedFileMeta, LanguageCount, OrphanChunkRow, OrphanFileSummary, ProjectInfo, SearchResult,
    SimilarityNeighborRow, TextSearchResult, TopicCentroidRow, TopicCoverageRow,
};

#[async_trait]
impl DbClient for PgPool {
    fn pool(&self) -> Option<&PgPool> {
        Some(self)
    }

    async fn upsert_project(
        &self,
        workspace_path: &str,
        path: &str,
        name: &str,
        git_common_dir: Option<&str>,
        git_root_commits: Option<&str>,
    ) -> Result<i32, sqlx::Error> {
        queries::upsert_project(
            self,
            workspace_path,
            path,
            name,
            git_common_dir,
            git_root_commits,
        )
        .await
    }

    async fn list_projects(&self) -> Result<Vec<ProjectInfo>, sqlx::Error> {
        queries::list_projects(self).await
    }

    async fn find_project_by_cwd(&self, cwd: &str) -> Result<Option<ProjectInfo>, sqlx::Error> {
        queries::find_project_by_cwd(self, cwd).await
    }

    async fn language_summary(
        &self,
        project_name: &str,
    ) -> Result<Vec<LanguageCount>, sqlx::Error> {
        queries::language_summary(self, project_name).await
    }

    async fn update_project_scanned(&self, project_id: i32) -> Result<(), sqlx::Error> {
        queries::update_project_scanned(self, project_id).await
    }

    async fn update_projects_scanned_by_workspace(
        &self,
        workspace_path: &str,
    ) -> Result<u64, sqlx::Error> {
        queries::update_projects_scanned_by_workspace(self, workspace_path).await
    }

    async fn delete_projects_by_workspace(&self, workspace_path: &str) -> Result<u64, sqlx::Error> {
        queries::delete_projects_by_workspace(self, workspace_path).await
    }

    async fn cleanup_orphaned_projects(&self) -> Result<u64, sqlx::Error> {
        queries::cleanup_orphaned_projects(self).await
    }

    async fn list_project_names(&self) -> Result<Vec<String>, sqlx::Error> {
        queries::list_project_names(self).await
    }

    async fn get_all_file_metadata(&self) -> Result<Vec<IndexedFileMeta>, sqlx::Error> {
        queries::get_all_file_metadata(self).await
    }

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
    ) -> Result<i64, sqlx::Error> {
        queries::upsert_file(
            self,
            project_id,
            path,
            relative_path,
            language,
            size_bytes,
            content,
            content_hash,
            line_count,
            truncated,
            content_recoverable_from_disk,
            modified_at,
        )
        .await
    }

    async fn get_content_hash(&self, path: &str) -> Result<Option<i64>, sqlx::Error> {
        queries::get_content_hash(self, path).await
    }

    async fn finalize_file_hash(&self, file_id: i64, content_hash: i64) -> Result<(), sqlx::Error> {
        queries::finalize_file_hash(self, file_id, content_hash).await
    }

    async fn delete_file_chunks(&self, file_id: i64) -> Result<(), sqlx::Error> {
        queries::delete_file_chunks(self, file_id).await
    }

    async fn insert_chunk(
        &self,
        file_id: i64,
        chunk_index: i32,
        content: &str,
        start_line: i32,
        end_line: i32,
        embedding: &[f32],
    ) -> Result<(), sqlx::Error> {
        queries::insert_chunk(
            self,
            file_id,
            chunk_index,
            content,
            start_line,
            end_line,
            embedding,
        )
        .await
    }

    async fn insert_chunks_batch(
        &self,
        file_id: i64,
        chunks: &[queries::ChunkInsert<'_>],
    ) -> Result<queries::ChunkBatchOutcome, sqlx::Error> {
        queries::insert_chunks_batch(self, file_id, chunks).await
    }

    async fn replace_indexed_file(
        &self,
        replacement: queries::IndexedFileReplacement<'_>,
    ) -> Result<i64, sqlx::Error> {
        queries::replace_indexed_file(self, replacement).await
    }

    async fn delete_file(&self, path: &str) -> Result<(), sqlx::Error> {
        queries::delete_file(self, path).await
    }

    async fn delete_files_batch(&self, paths: &[String]) -> Result<u64, sqlx::Error> {
        queries::delete_files_batch(self, paths).await
    }

    async fn delete_files_by_language(&self, language: &str) -> Result<u64, sqlx::Error> {
        queries::delete_files_by_language(self, language).await
    }

    async fn get_file_id_by_path(&self, path: &str) -> Result<Option<i64>, sqlx::Error> {
        queries::get_file_id_by_path(self, path).await
    }

    async fn get_file_line_count(&self, file_id: i64) -> Result<i32, sqlx::Error> {
        queries::get_file_line_count(self, file_id).await
    }

    async fn cleanup_stale_files(&self) -> Result<u64, sqlx::Error> {
        queries::cleanup_stale_files(self).await
    }

    async fn semantic_search(
        &self,
        embedding: &[f32],
        limit: i32,
        language: Option<&str>,
        project: Option<&str>,
        ef_search: i32,
        dedupe_worktrees: bool,
    ) -> Result<Vec<SearchResult>, sqlx::Error> {
        queries::semantic_search(
            self,
            embedding,
            limit,
            language,
            project,
            ef_search,
            dedupe_worktrees,
        )
        .await
    }

    async fn text_search(
        &self,
        query: &str,
        limit: i32,
        language: Option<&str>,
        project: Option<&str>,
        dedupe_worktrees: bool,
    ) -> Result<Vec<TextSearchResult>, sqlx::Error> {
        queries::text_search(self, query, limit, language, project, dedupe_worktrees).await
    }

    async fn text_search_bounded(
        &self,
        query: &str,
        limit: i32,
        language: Option<&str>,
        project: Option<&str>,
        dedupe_worktrees: bool,
        statement_timeout_ms: u32,
    ) -> Result<Vec<TextSearchResult>, sqlx::Error> {
        queries::text_search_bounded(
            self,
            query,
            limit,
            language,
            project,
            dedupe_worktrees,
            statement_timeout_ms,
        )
        .await
    }

    async fn grep_search(
        &self,
        pattern: &str,
        glob: Option<&str>,
        limit: i32,
        dedupe_worktrees: bool,
    ) -> Result<Vec<GrepResult>, sqlx::Error> {
        queries::grep_search(self, pattern, glob, limit, dedupe_worktrees).await
    }

    async fn read_file(&self, path: &str) -> Result<Option<FileContent>, sqlx::Error> {
        queries::read_file(self, path).await
    }

    async fn read_file_by_relative_path(
        &self,
        relative_path: &str,
    ) -> Result<Option<FileContent>, sqlx::Error> {
        queries::read_file_by_relative_path(self, relative_path).await
    }

    async fn file_info(&self, path: &str) -> Result<Option<FileInfo>, sqlx::Error> {
        queries::file_info(self, path).await
    }

    async fn file_chunk_summary(
        &self,
        path: &str,
    ) -> Result<queries::FileChunkSummary, sqlx::Error> {
        queries::file_chunk_summary(self, path).await
    }

    async fn get_file_region_by_lines(
        &self,
        path: &str,
        start_line: i32,
        end_line: i32,
    ) -> Result<Vec<queries::FileChunkRow>, sqlx::Error> {
        queries::get_file_region_by_lines(self, path, start_line, end_line).await
    }

    async fn get_chunks_in_index_range(
        &self,
        path: &str,
        idx_start: i32,
        idx_end: i32,
    ) -> Result<Vec<queries::FileChunkRow>, sqlx::Error> {
        queries::get_chunks_in_index_range(self, path, idx_start, idx_end).await
    }

    async fn grep_search_chunks(
        &self,
        pattern: &str,
        project: Option<&str>,
        language: Option<&str>,
        glob: Option<&str>,
        case_insensitive: bool,
        limit: i32,
        dedupe_worktrees: bool,
    ) -> Result<Vec<queries::GrepChunkResult>, sqlx::Error> {
        queries::grep_search_chunks(
            self,
            pattern,
            project,
            language,
            glob,
            case_insensitive,
            limit,
            dedupe_worktrees,
        )
        .await
    }

    async fn find_canonical_by_content_hash(
        &self,
        project_id: i32,
        content_hash: i64,
    ) -> Result<Option<queries::CanonicalFileMatch>, sqlx::Error> {
        queries::find_canonical_by_content_hash(self, project_id, content_hash).await
    }

    async fn update_file_path_in_place(
        &self,
        file_id: i64,
        new_path: &str,
        new_relative_path: &str,
        modified_at: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        queries::update_file_path_in_place(self, file_id, new_path, new_relative_path, modified_at)
            .await
    }

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
    ) -> Result<i64, sqlx::Error> {
        queries::insert_duplicate_file(
            self,
            project_id,
            path,
            relative_path,
            language,
            size_bytes,
            content_hash,
            canonical_file_id,
            modified_at,
        )
        .await
    }

    async fn project_tree(
        &self,
        project_name: &str,
        depth: i32,
    ) -> Result<Vec<String>, sqlx::Error> {
        queries::project_tree(self, project_name, depth).await
    }

    async fn search_file_paths(
        &self,
        prefix: &str,
        limit: i32,
    ) -> Result<Vec<String>, sqlx::Error> {
        queries::search_file_paths(self, prefix, limit).await
    }

    async fn list_languages(&self) -> Result<Vec<String>, sqlx::Error> {
        queries::list_languages(self).await
    }

    async fn resolve_file_reference(
        &self,
        file_ref: &str,
    ) -> Result<Option<FileReference>, sqlx::Error> {
        queries::resolve_file_reference(self, file_ref).await
    }

    async fn find_files_by_path_pattern(
        &self,
        project: &str,
        pattern: &str,
    ) -> Result<Vec<FileReference>, sqlx::Error> {
        queries::find_files_by_path_pattern(self, project, pattern).await
    }

    async fn count_indexed_files(&self) -> Result<u64, sqlx::Error> {
        queries::count_indexed_files(self).await
    }

    async fn count_chunks(&self) -> Result<u64, sqlx::Error> {
        queries::count_chunks(self).await
    }

    async fn count_projects(&self) -> Result<u64, sqlx::Error> {
        queries::count_projects(self).await
    }

    async fn total_bytes_indexed(&self) -> Result<u64, sqlx::Error> {
        queries::total_bytes_indexed(self).await
    }

    async fn max_chunk_id(&self) -> Result<i64, sqlx::Error> {
        queries::max_chunk_id(self).await
    }

    async fn upsert_git_commit(
        &self,
        project_id: i32,
        commit_hash: &str,
        author: &str,
        author_date: DateTime<Utc>,
        subject: &str,
        body: Option<&str>,
    ) -> Result<i64, sqlx::Error> {
        queries::upsert_git_commit(
            self,
            project_id,
            commit_hash,
            author,
            author_date,
            subject,
            body,
        )
        .await
    }

    async fn insert_git_commit_chunk(
        &self,
        commit_id: i64,
        chunk_index: i32,
        content: &str,
        embedding: &[f32],
    ) -> Result<(), sqlx::Error> {
        queries::insert_git_commit_chunk(self, commit_id, chunk_index, content, embedding).await
    }

    async fn get_git_last_commit(&self, project_id: i32) -> Result<Option<String>, sqlx::Error> {
        queries::get_git_last_commit(self, project_id).await
    }

    async fn set_git_last_commit(&self, project_id: i32, sha: &str) -> Result<(), sqlx::Error> {
        queries::set_git_last_commit(self, project_id, sha).await
    }

    async fn update_blame_for_file(
        &self,
        file_id: i64,
        blame_commit: &str,
        blame_author: &str,
        blame_date: DateTime<Utc>,
        start_line: i32,
        end_line: i32,
    ) -> Result<(), sqlx::Error> {
        queries::update_blame_for_file(
            self,
            file_id,
            blame_commit,
            blame_author,
            blame_date,
            start_line,
            end_line,
        )
        .await
    }

    async fn semantic_search_commits(
        &self,
        embedding: &[f32],
        limit: i32,
        project: Option<&str>,
        ef_search: i32,
    ) -> Result<Vec<CommitSearchResult>, sqlx::Error> {
        queries::semantic_search_commits(self, embedding, limit, project, ef_search).await
    }

    async fn get_git_enabled_projects(&self) -> Result<Vec<(i32, String)>, sqlx::Error> {
        queries::get_git_enabled_projects(self).await
    }

    async fn insert_commit_file(
        &self,
        commit_id: i64,
        file_path: &str,
        change_type: char,
    ) -> Result<(), sqlx::Error> {
        queries::insert_commit_file(self, commit_id, file_path, change_type).await
    }

    async fn get_commits_missing_files(
        &self,
        project_id: i32,
    ) -> Result<Vec<(i64, String)>, sqlx::Error> {
        queries::get_commits_missing_files(self, project_id).await
    }

    async fn has_commit_files_for_project(&self, project: &str) -> Result<bool, sqlx::Error> {
        queries::has_commit_files_for_project(self, project).await
    }

    async fn compare_two_files(
        &self,
        file_id_a: i64,
        file_id_b: i64,
        ef_search: i32,
    ) -> Result<Vec<ChunkPairSimilarity>, sqlx::Error> {
        queries::compare_two_files(self, file_id_a, file_id_b, ef_search).await
    }

    async fn batch_find_cross_project_neighbors(
        &self,
        last_chunk_id: i64,
        batch_size: i32,
        top_k: i32,
        threshold: f64,
        ef_search: i32,
    ) -> Result<Vec<SimilarityNeighborRow>, sqlx::Error> {
        queries::batch_find_cross_project_neighbors(
            self,
            last_chunk_id,
            batch_size,
            top_k,
            threshold,
            ef_search,
        )
        .await
    }

    async fn insert_similarity_pairs(
        &self,
        rows: &[SimilarityNeighborRow],
    ) -> Result<u64, sqlx::Error> {
        queries::insert_similarity_pairs(self, rows).await
    }

    async fn clear_similarity_table(&self) -> Result<(), sqlx::Error> {
        queries::clear_similarity_table(self).await
    }

    async fn count_similarity_pairs(&self) -> Result<u64, sqlx::Error> {
        queries::count_similarity_pairs(self).await
    }

    async fn top_similar_file_pairs(
        &self,
        limit: i32,
    ) -> Result<Vec<FileSimilarityPair>, sqlx::Error> {
        queries::top_similar_file_pairs(self, limit).await
    }

    async fn find_similar_files(
        &self,
        file_id: i64,
        min_similarity: f64,
        limit: i32,
        target_project: Option<&str>,
        include_same_repo: bool,
    ) -> Result<Vec<FileSimilarityPair>, sqlx::Error> {
        queries::find_similar_files(
            self,
            file_id,
            min_similarity,
            limit,
            target_project,
            include_same_repo,
        )
        .await
    }

    async fn find_duplicate_file_pairs(
        &self,
        min_similarity: f64,
        language: Option<&str>,
        limit: i32,
        include_same_repo: bool,
    ) -> Result<Vec<DuplicateFilePair>, sqlx::Error> {
        queries::find_duplicate_file_pairs(self, min_similarity, language, limit, include_same_repo)
            .await
    }

    async fn bulk_extract_embeddings(
        &self,
        language: Option<&str>,
    ) -> Result<Vec<ChunkEmbeddingRow>, sqlx::Error> {
        queries::bulk_extract_embeddings(self, language).await
    }

    async fn bulk_extract_project_embeddings(
        &self,
        project_name: &str,
        language: Option<&str>,
    ) -> Result<Vec<ChunkEmbeddingRow>, sqlx::Error> {
        queries::bulk_extract_project_embeddings(self, project_name, language).await
    }

    async fn clear_topics_for_scope(&self, scope: &str) -> Result<(), sqlx::Error> {
        queries::clear_topics_for_scope(self, scope).await
    }

    async fn store_topics(&self, scope: &str, topics: &[TopicResult]) -> Result<(), sqlx::Error> {
        queries::store_topics(self, scope, topics).await
    }

    async fn load_cached_topics(
        &self,
        scope: &str,
        limit: i32,
    ) -> Result<Vec<serde_json::Value>, sqlx::Error> {
        queries::load_cached_topics(self, scope, limit).await
    }

    async fn has_topic_assignments(&self) -> Result<bool, sqlx::Error> {
        queries::has_topic_assignments(self).await
    }

    async fn load_topic_centroids(
        &self,
        scope: &str,
    ) -> Result<Vec<TopicCentroidRow>, sqlx::Error> {
        queries::load_topic_centroids(self, scope).await
    }

    async fn find_orphan_chunks(
        &self,
        project: Option<&str>,
        language: Option<&str>,
        limit: i32,
    ) -> Result<Vec<OrphanChunkRow>, sqlx::Error> {
        queries::find_orphan_chunks(self, project, language, limit).await
    }

    async fn find_orphan_file_summary(
        &self,
        project: Option<&str>,
    ) -> Result<Vec<OrphanFileSummary>, sqlx::Error> {
        queries::find_orphan_file_summary(self, project).await
    }

    async fn load_chunk_topic_assignments_for_files(
        &self,
        project: Option<&str>,
    ) -> Result<Vec<FileTopicRow>, sqlx::Error> {
        queries::load_chunk_topic_assignments_for_files(self, project).await
    }

    async fn find_coupled_files(
        &self,
        project: &str,
        min_coupling: f64,
        min_commits: i32,
    ) -> Result<Vec<CoupledFilePair>, sqlx::Error> {
        queries::find_coupled_files(self, project, min_coupling, min_commits).await
    }

    async fn get_file_complexity_data(
        &self,
        project: &str,
    ) -> Result<Vec<FileComplexityRow>, sqlx::Error> {
        queries::get_file_complexity_data(self, project).await
    }

    async fn get_test_topic_coverage(
        &self,
        project: &str,
    ) -> Result<Vec<TopicCoverageRow>, sqlx::Error> {
        queries::get_test_topic_coverage(self, project).await
    }

    async fn get_file_topic_distributions(
        &self,
        project: &str,
        language: Option<&str>,
    ) -> Result<Vec<FileTopicDistributionRow>, sqlx::Error> {
        queries::get_file_topic_distributions(self, project, language).await
    }

    async fn get_chunk_topic_details(
        &self,
        project: &str,
        language: Option<&str>,
    ) -> Result<Vec<ChunkTopicDetailRow>, sqlx::Error> {
        queries::get_chunk_topic_details(self, project, language).await
    }

    async fn get_doc_topic_coverage(
        &self,
        project: &str,
    ) -> Result<Vec<DocCoverageRow>, sqlx::Error> {
        queries::get_doc_topic_coverage(self, project).await
    }
}

// ============================================================================
// Compile-time tests: trait stays object-safe and Send + Sync.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Trait must remain object-safe so `Arc<dyn DbClient>` works in
    /// `SystemContext` (Phase 5) and in tool method signatures.
    fn _assert_object_safe(_: Box<dyn DbClient>) {}

    /// Trait must be `Send + Sync` so `Arc<dyn DbClient>` can cross
    /// `tokio::spawn` boundaries.
    fn _assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn trait_is_send_sync_and_object_safe() {
        _assert_send_sync::<Arc<dyn DbClient>>();
    }
}
