//! File processing pipeline: read -> xxHash3 -> check DB -> chunk -> embed -> upsert.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use crossbeam_channel::Sender;
use tracing::{debug, error, trace};
use xxhash_rust::xxh3::xxh3_64;

use crate::config::Config;
use crate::db::DbClient;
use crate::embed::pool::{ChunkData, EmbedIndexRequest, EmbedRequest};
use crate::indexer::{chunker, claude_chunker};
use crate::stats::tracker::StatsTracker;

/// Process a single file: read, hash, check if changed, chunk, embed, upsert.
#[allow(clippy::too_many_arguments)]
pub async fn process_file(
    path: &Path,
    project_id: i32,
    workspace_path: &str,
    config: &Config,
    db: &Arc<dyn DbClient>,
    embed_tx: &Sender<EmbedIndexRequest>,
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
    let metadata =
        std::fs::metadata(path).map_err(|e| crate::error::PgmcpError::file_io(path, e))?;
    let size_bytes = metadata.len() as i64;
    let modified_at: DateTime<Utc> = metadata
        .modified()
        .map_err(|e| crate::error::PgmcpError::file_io(path, e))?
        .into();

    let max_size = max_file_size_override.unwrap_or(config.indexer.max_file_size_bytes);

    // Pre-read size gate: for files exceeding max_size, skip reading content entirely.
    // Reading a 43 MB JSONL from 64 workers concurrently was the initial-scan OOM source.
    // Hash is derived from (size, mtime) so the file still registers stably in the index
    // and Level-1 skip (size+mtime match) short-circuits subsequent scans. No chunks, no
    // embeddings — the file exists in `indexed_files` as a placeholder, content-less row.
    if size_bytes > max_size as i64 {
        let mtime_nanos = modified_at.timestamp_nanos_opt().unwrap_or(0);
        let mut hash_buf = [0u8; 16];
        hash_buf[..8].copy_from_slice(&size_bytes.to_le_bytes());
        hash_buf[8..].copy_from_slice(&mtime_nanos.to_le_bytes());
        let content_hash = xxh3_64(&hash_buf) as i64;

        if let Ok(Some(existing_hash)) = db.get_content_hash(&path_str).await
            && existing_hash == content_hash
        {
            trace!(path = %path_str, "Large file unchanged, skipping");
            return Ok(());
        }

        let relative_path = path
            .strip_prefix(workspace_path)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();

        let file_id = db
            .upsert_file(
                project_id,
                &path_str,
                &relative_path,
                &language,
                size_bytes,
                None,
                Some(content_hash),
                0,
                true,
                modified_at,
            )
            .await?;

        db.delete_file_chunks(file_id).await?;

        debug!(
            path = %path_str,
            size_bytes,
            max_size,
            "File exceeds max_file_size_bytes; registered without reading content"
        );

        stats
            .files_indexed
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Ok(());
    }

    // Read file content
    let content =
        std::fs::read_to_string(path).map_err(|e| crate::error::PgmcpError::file_io(path, e))?;

    // Compute xxHash3
    let content_hash = xxh3_64(content.as_bytes()) as i64;

    // Check if content has changed
    if let Ok(Some(existing_hash)) = db.get_content_hash(&path_str).await
        && existing_hash == content_hash
    {
        trace!(path = %path_str, "File unchanged, skipping");
        return Ok(());
    }

    // Files ≤ max_size: store content inline, proceed to chunking.
    let stored_content = Some(content.as_str());
    let truncated = false;
    let line_count = content.lines().count() as i32;

    // Compute relative path
    let relative_path = path
        .strip_prefix(workspace_path)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();

    // Upsert file with NULL hash (two-phase commit: hash finalized after chunks)
    let file_id = db
        .upsert_file(
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
    db.delete_file_chunks(file_id).await?;

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
        db.finalize_file_hash(file_id, content_hash).await?;
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

    let request = EmbedIndexRequest::File(EmbedRequest {
        file_id,
        chunks: chunk_data,
        db: Arc::clone(db),
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
