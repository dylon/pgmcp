//! Mock implementations of pgmcp traits for unit tests.
//!
//! ## `MockDbClient`
//!
//! Implements `pgmcp::db::DbClient` with **typed public fields per query
//! method**. Tests populate the fields directly:
//!
//! ```ignore
//! let mut mock = MockDbClient::new();
//! mock.semantic_search_results = vec![row1, row2, row3];
//! let arc: Arc<dyn DbClient> = Arc::new(mock);
//! let result = arc.semantic_search(&[0.0; 384], 10, None, None, 100).await?;
//! assert_eq!(result.len(), 3);
//! ```
//!
//! Unfilled fields return their natural zero/empty value. Mutating state
//! (writes, upserts) is recorded in `*_calls: Mutex<Vec<…>>` collections so
//! tests can assert what was written. `Mutex` is `parking_lot` to keep the
//! mock cheap.

use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use pgmcp::cron::topic_clustering::TopicResult;
use pgmcp::db::DbClient;
use pgmcp::embed::EmbeddingBackend;
use pgmcp::error::Result as PgmcpResult;

use crate::fixtures::test_embedding;
use pgmcp::db::queries::{
    ChunkEmbeddingRow, ChunkPairSimilarity, ChunkTopicDetailRow, CommitSearchResult,
    CoupledFilePair, DocCoverageRow, DuplicateFilePair, FileComplexityRow, FileContent, FileInfo,
    FileReference, FileSimilarityPair, FileTopicDistributionRow, FileTopicRow, GrepResult,
    IndexedFileMeta, LanguageCount, OrphanChunkRow, OrphanFileSummary, ProjectInfo, SearchResult,
    SimilarityNeighborRow, TextSearchResult, TopicCentroidRow, TopicCoverageRow,
};

// ============================================================================
// Recorded calls — for write-side methods, tests inspect these to verify
// behaviour. Each entry is a tuple of the method's arguments (cloned).
// ============================================================================

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct UpsertProjectCall {
    pub workspace_path: String,
    pub path: String,
    pub name: String,
    pub git_common_dir: Option<String>,
    pub git_root_commits: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct InsertChunkCall {
    pub file_id: i64,
    pub chunk_index: i32,
    pub content: String,
    pub start_line: i32,
    pub end_line: i32,
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct UpsertFileCall {
    pub project_id: i32,
    pub path: String,
    pub relative_path: String,
    pub language: String,
    pub size_bytes: i64,
    pub content: Option<String>,
    pub content_hash: Option<i64>,
    pub line_count: i32,
    pub truncated: bool,
    pub content_recoverable_from_disk: bool,
    pub modified_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct StoreTopicsCall {
    pub scope: String,
    pub topic_count: usize,
}

// ============================================================================
// MockDbClient
// ============================================================================

/// Configurable test double for `DbClient`. Populate the public fields
/// before calling trait methods; recorded writes accumulate in `*_calls`
/// vectors that tests can inspect after the system-under-test runs.
#[derive(Default)]
pub struct MockDbClient {
    // Read-side: tests pre-populate these to control what the trait returns.
    pub projects: Vec<ProjectInfo>,
    pub project_by_cwd: Option<ProjectInfo>,
    pub language_summary_results: Vec<LanguageCount>,
    pub list_project_names_result: Vec<String>,
    pub indexed_file_metadata: Vec<IndexedFileMeta>,
    pub content_hashes: Vec<(String, Option<i64>)>,
    pub file_id_by_path: Vec<(String, Option<i64>)>,
    pub file_line_counts: Vec<(i64, i32)>,
    pub semantic_search_results: Vec<SearchResult>,
    pub text_search_results: Vec<TextSearchResult>,
    pub grep_search_results: Vec<GrepResult>,
    /// Override for the chunk-aware grep tool. When non-empty, the
    /// `grep_search_chunks` mock returns these verbatim instead of
    /// synthesizing from `grep_search_results`.
    pub grep_chunk_results_override: Vec<pgmcp::db::queries::GrepChunkResult>,
    pub read_file_result: Option<FileContent>,
    pub file_info_result: Option<FileInfo>,
    pub project_tree_result: Vec<String>,
    pub file_paths_result: Vec<String>,
    pub languages_result: Vec<String>,
    pub resolve_file_reference_result: Option<FileReference>,
    pub find_files_by_path_pattern_result: Vec<FileReference>,
    pub commit_search_results: Vec<CommitSearchResult>,
    pub git_enabled_projects: Vec<(i32, String)>,
    pub commits_missing_files: Vec<(i64, String)>,
    pub git_last_commit: Option<String>,
    pub compare_two_files_result: Vec<ChunkPairSimilarity>,
    pub batch_neighbors: Vec<SimilarityNeighborRow>,
    pub top_similar_file_pairs_result: Vec<FileSimilarityPair>,
    pub similar_files_result: Vec<FileSimilarityPair>,
    pub duplicate_file_pairs_result: Vec<DuplicateFilePair>,
    pub bulk_extract_embeddings_result: Vec<ChunkEmbeddingRow>,
    pub bulk_extract_project_embeddings_result: Vec<ChunkEmbeddingRow>,
    pub cached_topics: Vec<serde_json::Value>,
    pub topic_centroids: Vec<TopicCentroidRow>,
    pub orphan_chunks_result: Vec<OrphanChunkRow>,
    pub orphan_file_summary_result: Vec<OrphanFileSummary>,
    pub chunk_topic_assignments_for_files: Vec<FileTopicRow>,
    pub coupled_files_result: Vec<CoupledFilePair>,
    pub file_complexity_data: Vec<FileComplexityRow>,
    pub test_topic_coverage: Vec<TopicCoverageRow>,
    pub file_topic_distributions: Vec<FileTopicDistributionRow>,
    pub chunk_topic_details: Vec<ChunkTopicDetailRow>,
    pub doc_topic_coverage: Vec<DocCoverageRow>,

    // Scalar reads.
    pub count_indexed_files_result: u64,
    pub count_chunks_result: u64,
    pub count_projects_result: u64,
    pub total_bytes_indexed_result: u64,
    pub max_chunk_id_result: i64,
    pub similarity_pairs_count: u64,
    pub has_topic_assignments_result: bool,
    pub has_commit_files_for_project_result: bool,

    // Write-side: increment-only ID generators for upserts that return PKs.
    next_project_id: AtomicI32,
    next_file_id: AtomicI64,
    next_commit_id: AtomicI64,

    // Write-side: recorded calls.
    pub upsert_project_calls: Mutex<Vec<UpsertProjectCall>>,
    pub upsert_file_calls: Mutex<Vec<UpsertFileCall>>,
    pub insert_chunk_calls: Mutex<Vec<InsertChunkCall>>,
    pub store_topics_calls: Mutex<Vec<StoreTopicsCall>>,
    pub deleted_file_paths: Mutex<Vec<String>>,
    pub set_git_last_commit_calls: Mutex<Vec<(i32, String)>>,
}

impl MockDbClient {
    pub fn new() -> Self {
        Self {
            next_project_id: AtomicI32::new(1),
            next_file_id: AtomicI64::new(1),
            next_commit_id: AtomicI64::new(1),
            ..Default::default()
        }
    }
}

// ----------------------------------------------------------------------------
// DbClient impl. Read methods pull from the typed public fields; writes
// either bump an ID counter and return the new ID, or push into a recorded
// `*_calls` list. Methods we don't need a mock for return Default::default().
// ----------------------------------------------------------------------------

#[async_trait]
impl DbClient for MockDbClient {
    // -- projects -----------------------------------------------------------
    async fn upsert_project(
        &self,
        workspace_path: &str,
        path: &str,
        name: &str,
        git_common_dir: Option<&str>,
        git_root_commits: Option<&str>,
    ) -> Result<i32, sqlx::Error> {
        self.upsert_project_calls.lock().push(UpsertProjectCall {
            workspace_path: workspace_path.to_string(),
            path: path.to_string(),
            name: name.to_string(),
            git_common_dir: git_common_dir.map(|s| s.to_string()),
            git_root_commits: git_root_commits.map(|s| s.to_string()),
        });
        Ok(self.next_project_id.fetch_add(1, Ordering::SeqCst))
    }

    async fn list_projects(&self) -> Result<Vec<ProjectInfo>, sqlx::Error> {
        Ok(self.projects.clone())
    }

    async fn find_project_by_cwd(&self, _cwd: &str) -> Result<Option<ProjectInfo>, sqlx::Error> {
        Ok(self.project_by_cwd.clone())
    }

    async fn language_summary(
        &self,
        _project_name: &str,
    ) -> Result<Vec<LanguageCount>, sqlx::Error> {
        Ok(self.language_summary_results.clone())
    }

    async fn update_project_scanned(&self, _project_id: i32) -> Result<(), sqlx::Error> {
        Ok(())
    }

    async fn delete_projects_by_workspace(
        &self,
        _workspace_path: &str,
    ) -> Result<u64, sqlx::Error> {
        Ok(0)
    }

    async fn cleanup_orphaned_projects(&self) -> Result<u64, sqlx::Error> {
        Ok(0)
    }

    async fn list_project_names(&self) -> Result<Vec<String>, sqlx::Error> {
        Ok(self.list_project_names_result.clone())
    }

    // -- file metadata + chunks --------------------------------------------
    async fn get_all_file_metadata(&self) -> Result<Vec<IndexedFileMeta>, sqlx::Error> {
        Ok(self.indexed_file_metadata.clone())
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
        self.upsert_file_calls.lock().push(UpsertFileCall {
            project_id,
            path: path.to_string(),
            relative_path: relative_path.to_string(),
            language: language.to_string(),
            size_bytes,
            content: content.map(|s| s.to_string()),
            content_hash,
            line_count,
            truncated,
            content_recoverable_from_disk,
            modified_at,
        });
        Ok(self.next_file_id.fetch_add(1, Ordering::SeqCst))
    }

    async fn get_content_hash(&self, path: &str) -> Result<Option<i64>, sqlx::Error> {
        Ok(self
            .content_hashes
            .iter()
            .find(|(p, _)| p == path)
            .and_then(|(_, h)| *h))
    }

    async fn finalize_file_hash(
        &self,
        _file_id: i64,
        _content_hash: i64,
    ) -> Result<(), sqlx::Error> {
        Ok(())
    }

    async fn delete_file_chunks(&self, _file_id: i64) -> Result<(), sqlx::Error> {
        Ok(())
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
        self.insert_chunk_calls.lock().push(InsertChunkCall {
            file_id,
            chunk_index,
            content: content.to_string(),
            start_line,
            end_line,
            embedding: embedding.to_vec(),
        });
        Ok(())
    }

    async fn insert_chunks_batch(
        &self,
        file_id: i64,
        chunks: &[pgmcp::db::queries::ChunkInsert<'_>],
    ) -> Result<pgmcp::db::queries::ChunkBatchOutcome, sqlx::Error> {
        let mut log = self.insert_chunk_calls.lock();
        for c in chunks {
            log.push(InsertChunkCall {
                file_id,
                chunk_index: c.chunk_index,
                content: c.content.to_string(),
                start_line: c.start_line,
                end_line: c.end_line,
                embedding: c.embedding.to_vec(),
            });
        }
        Ok(pgmcp::db::queries::ChunkBatchOutcome {
            fk_violation: false,
            error: None,
        })
    }

    async fn delete_file(&self, path: &str) -> Result<(), sqlx::Error> {
        self.deleted_file_paths.lock().push(path.to_string());
        Ok(())
    }

    async fn delete_files_batch(&self, paths: &[String]) -> Result<u64, sqlx::Error> {
        let mut log = self.deleted_file_paths.lock();
        log.extend(paths.iter().cloned());
        Ok(paths.len() as u64)
    }

    async fn get_file_id_by_path(&self, path: &str) -> Result<Option<i64>, sqlx::Error> {
        Ok(self
            .file_id_by_path
            .iter()
            .find(|(p, _)| p == path)
            .and_then(|(_, id)| *id))
    }

    async fn get_file_line_count(&self, file_id: i64) -> Result<i32, sqlx::Error> {
        Ok(self
            .file_line_counts
            .iter()
            .find(|(id, _)| *id == file_id)
            .map(|(_, c)| *c)
            .unwrap_or(0))
    }

    async fn cleanup_stale_files(&self) -> Result<u64, sqlx::Error> {
        Ok(0)
    }

    // -- search ------------------------------------------------------------
    async fn semantic_search(
        &self,
        _embedding: &[f32],
        _limit: i32,
        _language: Option<&str>,
        _project: Option<&str>,
        _ef_search: i32,
        _dedupe_worktrees: bool,
    ) -> Result<Vec<SearchResult>, sqlx::Error> {
        Ok(self.semantic_search_results.clone())
    }

    async fn text_search(
        &self,
        _query: &str,
        _limit: i32,
        _language: Option<&str>,
        _dedupe_worktrees: bool,
    ) -> Result<Vec<TextSearchResult>, sqlx::Error> {
        Ok(self.text_search_results.clone())
    }

    async fn grep_search(
        &self,
        _pattern: &str,
        _glob: Option<&str>,
        _limit: i32,
        _dedupe_worktrees: bool,
    ) -> Result<Vec<GrepResult>, sqlx::Error> {
        Ok(self.grep_search_results.clone())
    }

    async fn read_file(&self, _path: &str) -> Result<Option<FileContent>, sqlx::Error> {
        Ok(self.read_file_result.clone())
    }

    async fn read_file_by_relative_path(
        &self,
        _relative_path: &str,
    ) -> Result<Option<FileContent>, sqlx::Error> {
        Ok(self.read_file_result.clone())
    }

    async fn file_info(&self, _path: &str) -> Result<Option<FileInfo>, sqlx::Error> {
        Ok(self.file_info_result.clone())
    }

    async fn file_chunk_summary(
        &self,
        _path: &str,
    ) -> Result<pgmcp::db::queries::FileChunkSummary, sqlx::Error> {
        Ok(pgmcp::db::queries::FileChunkSummary {
            chunk_count: 0,
            first_chunk_line: None,
            last_chunk_line: None,
        })
    }

    async fn get_file_region_by_lines(
        &self,
        _path: &str,
        _start_line: i32,
        _end_line: i32,
    ) -> Result<Vec<pgmcp::db::queries::FileChunkRow>, sqlx::Error> {
        Ok(Vec::new())
    }

    async fn get_chunks_in_index_range(
        &self,
        _path: &str,
        _idx_start: i32,
        _idx_end: i32,
    ) -> Result<Vec<pgmcp::db::queries::FileChunkRow>, sqlx::Error> {
        Ok(Vec::new())
    }

    async fn grep_search_chunks(
        &self,
        _pattern: &str,
        _project: Option<&str>,
        _language: Option<&str>,
        _glob: Option<&str>,
        _case_insensitive: bool,
        _limit: i32,
        _dedupe_worktrees: bool,
    ) -> Result<Vec<pgmcp::db::queries::GrepChunkResult>, sqlx::Error> {
        // Synthesize one `GrepChunkResult` per `grep_search_results`
        // entry so existing tests that stub `grep_search_results` (the
        // legacy whole-file API) keep working under the new chunk-aware
        // tool body. Tests that need precise chunk metadata can stub
        // `grep_chunk_results_override` instead.
        if !self.grep_chunk_results_override.is_empty() {
            return Ok(self.grep_chunk_results_override.clone());
        }
        let synthesized: Vec<pgmcp::db::queries::GrepChunkResult> = self
            .grep_search_results
            .iter()
            .enumerate()
            .map(|(i, r)| pgmcp::db::queries::GrepChunkResult {
                project_name: "mock".into(),
                path: r.path.clone(),
                relative_path: r.relative_path.clone(),
                language: r.language.clone(),
                chunk_index: 0,
                start_line: 1,
                end_line: 1,
                content: r.content.clone().unwrap_or_default() + if i > 0 { "" } else { "" },
            })
            .collect();
        Ok(synthesized)
    }

    async fn find_canonical_by_content_hash(
        &self,
        _project_id: i32,
        _content_hash: i64,
    ) -> Result<Option<pgmcp::db::queries::CanonicalFileMatch>, sqlx::Error> {
        Ok(None)
    }

    async fn update_file_path_in_place(
        &self,
        _file_id: i64,
        _new_path: &str,
        _new_relative_path: &str,
        _modified_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), sqlx::Error> {
        Ok(())
    }

    async fn insert_duplicate_file(
        &self,
        _project_id: i32,
        _path: &str,
        _relative_path: &str,
        _language: &str,
        _size_bytes: i64,
        _content_hash: i64,
        _canonical_file_id: i64,
        _modified_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<i64, sqlx::Error> {
        Ok(0)
    }

    async fn project_tree(
        &self,
        _project_name: &str,
        _depth: i32,
    ) -> Result<Vec<String>, sqlx::Error> {
        Ok(self.project_tree_result.clone())
    }

    async fn search_file_paths(
        &self,
        _prefix: &str,
        _limit: i32,
    ) -> Result<Vec<String>, sqlx::Error> {
        Ok(self.file_paths_result.clone())
    }

    async fn list_languages(&self) -> Result<Vec<String>, sqlx::Error> {
        Ok(self.languages_result.clone())
    }

    async fn resolve_file_reference(
        &self,
        _file_ref: &str,
    ) -> Result<Option<FileReference>, sqlx::Error> {
        Ok(self.resolve_file_reference_result.clone())
    }

    async fn find_files_by_path_pattern(
        &self,
        _project: &str,
        _pattern: &str,
    ) -> Result<Vec<FileReference>, sqlx::Error> {
        Ok(self.find_files_by_path_pattern_result.clone())
    }

    // -- statistics --------------------------------------------------------
    async fn count_indexed_files(&self) -> Result<u64, sqlx::Error> {
        Ok(self.count_indexed_files_result)
    }
    async fn count_chunks(&self) -> Result<u64, sqlx::Error> {
        Ok(self.count_chunks_result)
    }
    async fn count_projects(&self) -> Result<u64, sqlx::Error> {
        Ok(self.count_projects_result)
    }
    async fn total_bytes_indexed(&self) -> Result<u64, sqlx::Error> {
        Ok(self.total_bytes_indexed_result)
    }
    async fn max_chunk_id(&self) -> Result<i64, sqlx::Error> {
        Ok(self.max_chunk_id_result)
    }

    // -- git history -------------------------------------------------------
    async fn upsert_git_commit(
        &self,
        _project_id: i32,
        _commit_hash: &str,
        _author: &str,
        _author_date: DateTime<Utc>,
        _subject: &str,
        _body: Option<&str>,
    ) -> Result<i64, sqlx::Error> {
        Ok(self.next_commit_id.fetch_add(1, Ordering::SeqCst))
    }

    async fn insert_git_commit_chunk(
        &self,
        _commit_id: i64,
        _chunk_index: i32,
        _content: &str,
        _embedding: &[f32],
    ) -> Result<(), sqlx::Error> {
        Ok(())
    }

    async fn get_git_last_commit(&self, _project_id: i32) -> Result<Option<String>, sqlx::Error> {
        Ok(self.git_last_commit.clone())
    }

    async fn set_git_last_commit(&self, project_id: i32, sha: &str) -> Result<(), sqlx::Error> {
        self.set_git_last_commit_calls
            .lock()
            .push((project_id, sha.to_string()));
        Ok(())
    }

    async fn update_blame_for_file(
        &self,
        _file_id: i64,
        _blame_commit: &str,
        _blame_author: &str,
        _blame_date: DateTime<Utc>,
        _start_line: i32,
        _end_line: i32,
    ) -> Result<(), sqlx::Error> {
        Ok(())
    }

    async fn semantic_search_commits(
        &self,
        _embedding: &[f32],
        _limit: i32,
        _project: Option<&str>,
        _ef_search: i32,
    ) -> Result<Vec<CommitSearchResult>, sqlx::Error> {
        Ok(self.commit_search_results.clone())
    }

    async fn get_git_enabled_projects(&self) -> Result<Vec<(i32, String)>, sqlx::Error> {
        Ok(self.git_enabled_projects.clone())
    }

    async fn insert_commit_file(
        &self,
        _commit_id: i64,
        _file_path: &str,
        _change_type: char,
    ) -> Result<(), sqlx::Error> {
        Ok(())
    }

    async fn get_commits_missing_files(
        &self,
        _project_id: i32,
    ) -> Result<Vec<(i64, String)>, sqlx::Error> {
        Ok(self.commits_missing_files.clone())
    }

    async fn has_commit_files_for_project(&self, _project: &str) -> Result<bool, sqlx::Error> {
        Ok(self.has_commit_files_for_project_result)
    }

    // -- cross-project similarity ------------------------------------------
    async fn compare_two_files(
        &self,
        _file_id_a: i64,
        _file_id_b: i64,
        _ef_search: i32,
    ) -> Result<Vec<ChunkPairSimilarity>, sqlx::Error> {
        Ok(self.compare_two_files_result.clone())
    }

    async fn batch_find_cross_project_neighbors(
        &self,
        _last_chunk_id: i64,
        _batch_size: i32,
        _top_k: i32,
        _threshold: f64,
        _ef_search: i32,
    ) -> Result<Vec<SimilarityNeighborRow>, sqlx::Error> {
        Ok(self.batch_neighbors.clone())
    }

    async fn insert_similarity_pairs(
        &self,
        rows: &[SimilarityNeighborRow],
    ) -> Result<u64, sqlx::Error> {
        Ok(rows.len() as u64)
    }

    async fn clear_similarity_table(&self) -> Result<(), sqlx::Error> {
        Ok(())
    }

    async fn count_similarity_pairs(&self) -> Result<u64, sqlx::Error> {
        Ok(self.similarity_pairs_count)
    }

    async fn top_similar_file_pairs(
        &self,
        _limit: i32,
    ) -> Result<Vec<FileSimilarityPair>, sqlx::Error> {
        Ok(self.top_similar_file_pairs_result.clone())
    }

    async fn find_similar_files(
        &self,
        _file_id: i64,
        _min_similarity: f64,
        _limit: i32,
        _target_project: Option<&str>,
        _include_same_repo: bool,
    ) -> Result<Vec<FileSimilarityPair>, sqlx::Error> {
        Ok(self.similar_files_result.clone())
    }

    async fn find_duplicate_file_pairs(
        &self,
        _min_similarity: f64,
        _language: Option<&str>,
        _limit: i32,
        _include_same_repo: bool,
    ) -> Result<Vec<DuplicateFilePair>, sqlx::Error> {
        Ok(self.duplicate_file_pairs_result.clone())
    }

    // -- topic clustering --------------------------------------------------
    async fn bulk_extract_embeddings(
        &self,
        _language: Option<&str>,
    ) -> Result<Vec<ChunkEmbeddingRow>, sqlx::Error> {
        Ok(self.bulk_extract_embeddings_result.clone())
    }

    async fn bulk_extract_project_embeddings(
        &self,
        _project_name: &str,
        _language: Option<&str>,
    ) -> Result<Vec<ChunkEmbeddingRow>, sqlx::Error> {
        Ok(self.bulk_extract_project_embeddings_result.clone())
    }

    async fn clear_topics_for_scope(&self, _scope: &str) -> Result<(), sqlx::Error> {
        Ok(())
    }

    async fn store_topics(&self, scope: &str, topics: &[TopicResult]) -> Result<(), sqlx::Error> {
        self.store_topics_calls.lock().push(StoreTopicsCall {
            scope: scope.to_string(),
            topic_count: topics.len(),
        });
        Ok(())
    }

    async fn load_cached_topics(
        &self,
        _scope: &str,
        _limit: i32,
    ) -> Result<Vec<serde_json::Value>, sqlx::Error> {
        Ok(self.cached_topics.clone())
    }

    async fn has_topic_assignments(&self) -> Result<bool, sqlx::Error> {
        Ok(self.has_topic_assignments_result)
    }

    async fn load_topic_centroids(
        &self,
        _scope: &str,
    ) -> Result<Vec<TopicCentroidRow>, sqlx::Error> {
        Ok(self.topic_centroids.clone())
    }

    // -- analysis tools ----------------------------------------------------
    async fn find_orphan_chunks(
        &self,
        _project: Option<&str>,
        _language: Option<&str>,
        _limit: i32,
    ) -> Result<Vec<OrphanChunkRow>, sqlx::Error> {
        Ok(self.orphan_chunks_result.clone())
    }

    async fn find_orphan_file_summary(
        &self,
        _project: Option<&str>,
    ) -> Result<Vec<OrphanFileSummary>, sqlx::Error> {
        Ok(self.orphan_file_summary_result.clone())
    }

    async fn load_chunk_topic_assignments_for_files(
        &self,
        _project: Option<&str>,
    ) -> Result<Vec<FileTopicRow>, sqlx::Error> {
        Ok(self.chunk_topic_assignments_for_files.clone())
    }

    async fn find_coupled_files(
        &self,
        _project: &str,
        _min_coupling: f64,
        _min_commits: i32,
    ) -> Result<Vec<CoupledFilePair>, sqlx::Error> {
        Ok(self.coupled_files_result.clone())
    }

    async fn get_file_complexity_data(
        &self,
        _project: &str,
    ) -> Result<Vec<FileComplexityRow>, sqlx::Error> {
        Ok(self.file_complexity_data.clone())
    }

    async fn get_test_topic_coverage(
        &self,
        _project: &str,
    ) -> Result<Vec<TopicCoverageRow>, sqlx::Error> {
        Ok(self.test_topic_coverage.clone())
    }

    async fn get_file_topic_distributions(
        &self,
        _project: &str,
        _language: Option<&str>,
    ) -> Result<Vec<FileTopicDistributionRow>, sqlx::Error> {
        Ok(self.file_topic_distributions.clone())
    }

    async fn get_chunk_topic_details(
        &self,
        _project: &str,
        _language: Option<&str>,
    ) -> Result<Vec<ChunkTopicDetailRow>, sqlx::Error> {
        Ok(self.chunk_topic_details.clone())
    }

    async fn get_doc_topic_coverage(
        &self,
        _project: &str,
    ) -> Result<Vec<DocCoverageRow>, sqlx::Error> {
        Ok(self.doc_topic_coverage.clone())
    }
}

// ----------------------------------------------------------------------------
// Smoke test: prove the mock plugs into Arc<dyn DbClient> end-to-end.
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn populate_then_roundtrip_through_arc_dyn() {
        let mut mock = MockDbClient::new();
        mock.projects.push(ProjectInfo {
            id: 7,
            workspace_path: "/ws".into(),
            path: "/ws/foo".into(),
            name: "foo".into(),
            discovered_at: None,
            last_scanned_at: None,
            file_count: Some(123),
            git_common_dir: None,
            git_root_commits: None,
        });

        let arc: Arc<dyn DbClient> = Arc::new(mock);
        let projects = arc.list_projects().await.expect("list_projects");
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id, 7);
        assert_eq!(projects[0].name, "foo");
    }

    #[tokio::test]
    async fn write_methods_record_calls() {
        let mock = MockDbClient::new();
        let id = mock
            .upsert_project("/ws", "/ws/x", "x", None, None)
            .await
            .expect("upsert_project");
        assert_eq!(id, 1, "first id");
        assert_eq!(mock.upsert_project_calls.lock().len(), 1);

        let id2 = mock
            .upsert_project("/ws", "/ws/y", "y", None, None)
            .await
            .expect("second upsert");
        assert_eq!(id2, 2);
        assert_eq!(mock.upsert_project_calls.lock().len(), 2);
    }
}

// ============================================================================
// DeterministicEmbeddingBackend
// ============================================================================

/// Test backend for `EmbeddingBackend`. Returns a deterministic, L2-normalized
/// f32 vector keyed by the hash of the input text. Uses
/// `crate::fixtures::test_embedding` so two calls with the same text always
/// return the same vector — useful for asserting end-to-end pipelines.
///
/// `dim` defaults to 384 to match the production fastembed model.
pub struct DeterministicEmbeddingBackend {
    pub dim: usize,
}

impl DeterministicEmbeddingBackend {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

impl Default for DeterministicEmbeddingBackend {
    fn default() -> Self {
        Self::new(384)
    }
}

#[async_trait]
impl EmbeddingBackend for DeterministicEmbeddingBackend {
    async fn embed_one(&self, text: &str) -> PgmcpResult<Vec<f32>> {
        Ok(test_embedding(self.dim, text))
    }

    fn name(&self) -> &'static str {
        "deterministic"
    }
}

#[cfg(test)]
mod backend_tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn deterministic_backend_matches_fixture() {
        let backend = DeterministicEmbeddingBackend::new(384);
        let arc: Arc<dyn EmbeddingBackend> = Arc::new(backend);
        let v1 = arc.embed_one("alpha").await.expect("embed_one");
        let v2 = arc.embed_one("alpha").await.expect("embed_one");
        assert_eq!(v1, v2);
        assert_eq!(v1.len(), 384);
        let v3 = arc.embed_one("beta").await.expect("embed_one");
        assert_ne!(v1, v3);
    }
}
