//! File processing pipeline: read -> xxHash3 -> check DB -> chunk -> embed -> upsert.

use std::path::Path;

use chrono::{DateTime, Utc};
use crossbeam_channel::Sender;
use tracing::{debug, error, trace};
use xxhash_rust::xxh3::xxh3_64;

use crate::config::Config;
use crate::db;
use crate::embed::pool::{ChunkData, EmbedRequest, EmbedRequestKind};
use crate::indexer::{chunker, claude_chunker};
use crate::stats::tracker::StatsTracker;

/// Process a single file: read, hash, check if changed, chunk, embed, upsert.
pub async fn process_file(
    path: &Path,
    project_id: i32,
    workspace_path: &str,
    config: &Config,
    db_pool: &sqlx::PgPool,
    embed_tx: &Sender<EmbedRequestKind>,
    stats: &StatsTracker,
    max_file_size_override: Option<u64>,
) -> Result<(), crate::error::PgmcpError> {
    let path_str = path.to_string_lossy();

    // Get language for this file
    let language = match config.indexer.language_for_path(path) {
        Some(lang) => lang,
        None => {
            trace!(path = %path_str, "Skipping file: unconfigured extension");
            return Ok(());
        }
    };

    // Read file metadata
    let metadata = std::fs::metadata(path).map_err(|e| crate::error::PgmcpError::file_io(path, e))?;
    let size_bytes = metadata.len() as i64;
    let modified_at: DateTime<Utc> = metadata
        .modified()
        .map_err(|e| crate::error::PgmcpError::file_io(path, e))?
        .into();

    // Read file content
    let content = std::fs::read_to_string(path).map_err(|e| crate::error::PgmcpError::file_io(path, e))?;

    // Compute xxHash3
    let content_hash = xxh3_64(content.as_bytes()) as i64;

    // Check if content has changed
    if let Ok(Some(existing_hash)) = db::queries::get_content_hash(db_pool, &path_str).await {
        if existing_hash == content_hash {
            trace!(path = %path_str, "File unchanged, skipping");
            return Ok(());
        }
    }

    // Determine if file should be truncated
    let max_size = max_file_size_override.unwrap_or(config.indexer.max_file_size_bytes);
    let truncated = size_bytes > max_size as i64;
    let stored_content = if truncated {
        None
    } else {
        Some(content.as_str())
    };
    let line_count = content.lines().count() as i32;

    // Compute relative path
    let relative_path = path
        .strip_prefix(workspace_path)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();

    // Upsert file with NULL hash (two-phase commit: hash finalized after chunks)
    let file_id = db::queries::upsert_file(
        db_pool,
        project_id,
        &path_str,
        &relative_path,
        &language,
        size_bytes,
        stored_content,
        None,
        line_count,
        truncated,
        modified_at,
    )
    .await?;

    // Delete old chunks
    db::queries::delete_file_chunks(db_pool, file_id).await?;

    // Chunk the content, routing to the appropriate chunker
    let chunks = if &*language == "jsonl" && claude_chunker::is_claude_session_transcript(path) {
        claude_chunker::chunk_claude_jsonl(&content)
    } else if &*language == "jsonl" {
        chunker::chunk_jsonl_content(&content)
    } else {
        chunker::chunk_content(
            &content,
            config.embeddings.chunk_size_lines,
            config.embeddings.chunk_overlap_lines,
        )
    };

    if chunks.is_empty() {
        // No chunks to embed — finalize hash immediately
        db::queries::finalize_file_hash(db_pool, file_id, content_hash).await?;
        return Ok(());
    }

    // Submit chunks for embedding
    let chunk_data: Vec<ChunkData> = chunks
        .into_iter()
        .map(|c| ChunkData {
            chunk_index: c.chunk_index,
            content: c.content,
            start_line: c.start_line,
            end_line: c.end_line,
        })
        .collect();

    let request = EmbedRequestKind::File(EmbedRequest {
        file_id,
        chunks: chunk_data,
        db_pool: db_pool.clone(),
        content_hash,
    });

    if let Err(e) = embed_tx.send(request) {
        error!(path = %path_str, error = %e, "Failed to submit embedding request");
    }

    stats
        .files_indexed
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    stats
        .bytes_processed
        .fetch_add(size_bytes as u64, std::sync::atomic::Ordering::Relaxed);

    debug!(path = %path_str, language, line_count, "File indexed");
    Ok(())
}
