//! Git history indexer: extracts commit messages, diffs, and blame metadata.
//!
//! Runs as a background cron job for projects with `git.index_history = true`
//! in their `.pgmcp.toml`. Tracks the last-indexed commit SHA per project
//! in `pgmcp_metadata` to enable incremental updates.

use std::path::Path;
use std::process::Command;

use chrono::{DateTime, Utc};
use crossbeam_channel::Sender;
use tracing::{debug, error, info, warn};

use crate::config::ProjectOverride;
use crate::db;
use crate::embed::pool::{ChunkData, EmbedCommitRequest};
use crate::stats::tracker::StatsTracker;

/// Maximum diff size (in bytes) per commit before we skip the diff and only index the message.
const MAX_DIFF_BYTES: usize = 100_000;

/// Separator between fields in git log --format output.
const FIELD_SEP: &str = "\x1f"; // ASCII Unit Separator
/// Separator between commits in git log output.
const COMMIT_SEP: &str = "\x1e"; // ASCII Record Separator

/// Index git history for a project. Only processes commits newer than the last indexed SHA.
pub async fn index_git_history(
    project_root: &Path,
    project_id: i32,
    db_pool: &sqlx::PgPool,
    embed_tx: &Sender<EmbedCommitRequest>,
    stats: &StatsTracker,
) -> Result<(), crate::error::PgmcpError> {
    // Check if the project has a .git/ directory
    if !project_root.join(".git").is_dir() {
        return Ok(());
    }

    // Get last indexed commit
    let last_sha = db::queries::get_git_last_commit(db_pool, project_id)
        .await
        .unwrap_or(None);

    // Build git log command
    let format_str = format!(
        "{}%H{}%an{}%aI{}%s{}%b{}",
        COMMIT_SEP, FIELD_SEP, FIELD_SEP, FIELD_SEP, FIELD_SEP, FIELD_SEP
    );

    let mut cmd = Command::new("git");
    cmd.current_dir(project_root)
        .arg("log")
        .arg(format!("--format={}", format_str))
        .arg("--no-merges")
        .arg("--max-count=500"); // Safety limit per run

    if let Some(ref sha) = last_sha {
        cmd.arg(format!("{}..HEAD", sha));
    }

    let output = cmd.output().map_err(|e| {
        crate::error::PgmcpError::Other(format!(
            "Failed to run git log in {}: {}",
            project_root.display(),
            e
        ))
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(project = %project_root.display(), stderr = %stderr, "git log failed");
        return Ok(());
    }

    let log_output = String::from_utf8_lossy(&output.stdout);
    let commits = parse_git_log(&log_output);

    if commits.is_empty() {
        debug!(project = %project_root.display(), "No new commits to index");
        return Ok(());
    }

    info!(
        project = %project_root.display(),
        count = commits.len(),
        "Indexing git history"
    );

    let mut newest_sha: Option<String> = None;

    for commit in &commits {
        // Track the newest commit (first in output, since git log is reverse chronological)
        if newest_sha.is_none() {
            newest_sha = Some(commit.hash.clone());
        }

        // Get the diff for this commit
        let diff = get_commit_diff(project_root, &commit.hash);

        // Build chunk content: commit message + diff
        let mut chunk_text = format!(
            "commit {}\nAuthor: {}\nDate: {}\n\n{}\n",
            commit.hash, commit.author, commit.date, commit.subject
        );
        if !commit.body.is_empty() {
            chunk_text.push_str(&commit.body);
            chunk_text.push('\n');
        }
        if let Some(ref diff_text) = diff
            && diff_text.len() <= MAX_DIFF_BYTES
        {
            chunk_text.push_str("\n---\n");
            chunk_text.push_str(diff_text);
        }

        // Upsert commit in DB
        let author_date = commit
            .date
            .parse::<DateTime<Utc>>()
            .unwrap_or_else(|_| Utc::now());

        let commit_id = match db::queries::upsert_git_commit(
            db_pool,
            project_id,
            &commit.hash,
            &commit.author,
            author_date,
            &commit.subject,
            if commit.body.is_empty() {
                None
            } else {
                Some(&commit.body)
            },
        )
        .await
        {
            Ok(id) => id,
            Err(e) => {
                error!(hash = %commit.hash, error = %e, "Failed to upsert git commit");
                stats
                    .git_commits_failed
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                continue;
            }
        };

        // Index files changed in this commit
        if let Err(e) = index_commit_files(db_pool, project_root, commit_id, &commit.hash).await {
            debug!(hash = %commit.hash, error = %e, "Failed to index commit files");
        }

        // Submit for embedding
        let chunk = ChunkData {
            chunk_index: 0,
            content: chunk_text,
            start_line: 0,
            end_line: 0,
        };

        let request = EmbedCommitRequest {
            commit_id,
            chunks: vec![chunk],
            db_pool: db_pool.clone(),
        };

        if let Err(e) = embed_tx.send(request) {
            error!(hash = %commit.hash, error = %e, "Failed to submit commit for embedding");
            stats
                .git_commits_failed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        } else {
            stats
                .git_commits_indexed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    // Backfill git_commit_files for any commits that were previously indexed
    // without file change tracking
    backfill_commit_files(db_pool, project_root, project_id).await;

    // Update last indexed commit
    if let Some(sha) = newest_sha
        && let Err(e) = db::queries::set_git_last_commit(db_pool, project_id, &sha).await
    {
        error!(project_id, error = %e, "Failed to update last indexed commit");
    }

    Ok(())
}

/// Update blame metadata for a file's chunks.
#[allow(dead_code)]
pub async fn update_blame_metadata(
    project_root: &Path,
    file_path: &Path,
    file_id: i64,
    db_pool: &sqlx::PgPool,
) -> Result<(), crate::error::PgmcpError> {
    let relative = file_path.strip_prefix(project_root).unwrap_or(file_path);

    let output = Command::new("git")
        .current_dir(project_root)
        .arg("blame")
        .arg("--line-porcelain")
        .arg(relative.to_string_lossy().as_ref())
        .output()
        .map_err(|e| {
            crate::error::PgmcpError::Other(format!(
                "Failed to run git blame on {}: {}",
                file_path.display(),
                e
            ))
        })?;

    if !output.status.success() {
        return Ok(()); // File may not be tracked
    }

    let blame_output = String::from_utf8_lossy(&output.stdout);
    let blame_entries = parse_blame_porcelain(&blame_output);

    for entry in &blame_entries {
        if let Err(e) = db::queries::update_blame_for_file(
            db_pool,
            file_id,
            &entry.commit_hash,
            &entry.author,
            entry.date,
            entry.start_line,
            entry.end_line,
        )
        .await
        {
            debug!(file_id, error = %e, "Failed to update blame metadata");
        }
    }

    Ok(())
}

/// Check if a project has git history indexing enabled via `.pgmcp.toml`.
pub fn is_git_history_enabled(project_root: &Path) -> bool {
    ProjectOverride::load(project_root)
        .and_then(|o| o.git)
        .map(|g| g.index_history)
        .unwrap_or(false)
}

/// Extract files changed in a commit using `git diff-tree` and store them.
async fn index_commit_files(
    db_pool: &sqlx::PgPool,
    repo_path: &Path,
    commit_db_id: i64,
    commit_hash: &str,
) -> Result<(), crate::error::PgmcpError> {
    let output = Command::new("git")
        .args([
            "diff-tree",
            "--no-commit-id",
            "--name-status",
            "-r",
            commit_hash,
        ])
        .current_dir(repo_path)
        .output()
        .map_err(|e| {
            crate::error::PgmcpError::Other(format!(
                "Failed to run git diff-tree for {}: {}",
                commit_hash, e
            ))
        })?;

    if !output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Format: "M\tsrc/foo.rs" or "R100\told\tnew" (rename with score)
        let parts: Vec<&str> = line.splitn(2, '\t').collect();
        if parts.len() < 2 {
            continue;
        }
        let change_type = parts[0].chars().next().unwrap_or('M');
        let file_path = if change_type == 'R' || change_type == 'C' {
            // For renames/copies, the new path is the second tab-delimited field
            parts[1].split('\t').next_back().unwrap_or(parts[1])
        } else {
            parts[1]
        };

        if let Err(e) =
            db::queries::insert_commit_file(db_pool, commit_db_id, file_path, change_type).await
        {
            debug!(
                commit_hash,
                file_path,
                error = %e,
                "Failed to insert commit file"
            );
        }
    }

    Ok(())
}

/// Backfill `git_commit_files` for commits that were indexed before file tracking was added.
async fn backfill_commit_files(db_pool: &sqlx::PgPool, repo_path: &Path, project_id: i32) {
    let missing = match db::queries::get_commits_missing_files(db_pool, project_id).await {
        Ok(rows) => rows,
        Err(e) => {
            debug!(project_id, error = %e, "Failed to query commits missing files");
            return;
        }
    };

    if missing.is_empty() {
        return;
    }

    info!(
        project_id,
        count = missing.len(),
        "Backfilling git_commit_files for previously indexed commits"
    );

    for (commit_db_id, commit_hash) in &missing {
        if let Err(e) = index_commit_files(db_pool, repo_path, *commit_db_id, commit_hash).await {
            debug!(commit_hash, error = %e, "Failed to backfill commit files");
        }
    }
}

// ============================================================================
// Internal helpers
// ============================================================================

#[derive(Debug)]
struct ParsedCommit {
    hash: String,
    author: String,
    date: String,
    subject: String,
    body: String,
}

fn parse_git_log(output: &str) -> Vec<ParsedCommit> {
    let mut commits = Vec::new();

    for record in output.split(COMMIT_SEP) {
        let record = record.trim();
        if record.is_empty() {
            continue;
        }

        let fields: Vec<&str> = record.split(FIELD_SEP).collect();
        if fields.len() < 5 {
            continue;
        }

        commits.push(ParsedCommit {
            hash: fields[0].trim().to_string(),
            author: fields[1].trim().to_string(),
            date: fields[2].trim().to_string(),
            subject: fields[3].trim().to_string(),
            body: fields[4].trim().to_string(),
        });
    }

    commits
}

fn get_commit_diff(project_root: &Path, sha: &str) -> Option<String> {
    let output = Command::new("git")
        .current_dir(project_root)
        .arg("show")
        .arg(sha)
        .arg("--format=")
        .arg("-p")
        .arg("--diff-filter=AMCR") // Added, Modified, Copied, Renamed (skip deleted)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let diff = String::from_utf8_lossy(&output.stdout).into_owned();
    if diff.trim().is_empty() {
        None
    } else {
        Some(diff)
    }
}

#[derive(Debug)]
struct BlameEntry {
    commit_hash: String,
    author: String,
    date: DateTime<Utc>,
    start_line: i32,
    end_line: i32,
}

fn parse_blame_porcelain(output: &str) -> Vec<BlameEntry> {
    let mut entries = Vec::new();
    let mut current_hash = String::new();
    let mut current_author = String::new();
    let mut current_timestamp: i64 = 0;
    let mut current_line: i32 = 0;
    let mut current_num_lines: i32 = 1;

    for line in output.lines() {
        if line.len() >= 40 && line.chars().take(40).all(|c| c.is_ascii_hexdigit()) {
            // Commit line: <hash> <orig-line> <final-line> [<num-lines>]
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                current_hash = parts[0].to_string();
                current_line = parts[2].parse().unwrap_or(0);
                current_num_lines = if parts.len() >= 4 {
                    parts[3].parse().unwrap_or(1)
                } else {
                    1
                };
            }
        } else if let Some(author) = line.strip_prefix("author ") {
            current_author = author.to_string();
        } else if let Some(ts) = line.strip_prefix("author-time ") {
            current_timestamp = ts.parse().unwrap_or(0);
        } else if line.starts_with('\t') {
            // Content line — this marks the end of a blame block
            if !current_hash.is_empty() && current_line > 0 {
                let date = DateTime::from_timestamp(current_timestamp, 0).unwrap_or_else(Utc::now);
                entries.push(BlameEntry {
                    commit_hash: current_hash.clone(),
                    author: current_author.clone(),
                    date,
                    start_line: current_line,
                    end_line: current_line + current_num_lines - 1,
                });
            }
        }
    }

    // Merge consecutive entries with the same commit
    entries.sort_by_key(|e| e.start_line);
    let mut merged: Vec<BlameEntry> = Vec::new();
    for entry in entries {
        if let Some(last) = merged.last_mut()
            && last.commit_hash == entry.commit_hash
            && last.end_line + 1 >= entry.start_line
        {
            last.end_line = last.end_line.max(entry.end_line);
            continue;
        }
        merged.push(entry);
    }

    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_git_log() {
        let output = format!(
            "{sep}abc123{fs}Alice{fs}2024-01-15T10:00:00+00:00{fs}Fix bug{fs}Detailed body{fs}",
            sep = COMMIT_SEP,
            fs = FIELD_SEP,
        );
        let commits = parse_git_log(&output);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].hash, "abc123");
        assert_eq!(commits[0].author, "Alice");
        assert_eq!(commits[0].subject, "Fix bug");
        assert_eq!(commits[0].body, "Detailed body");
    }

    #[test]
    fn test_parse_git_log_empty() {
        assert!(parse_git_log("").is_empty());
        assert!(parse_git_log("  \n  ").is_empty());
    }

    #[test]
    fn test_parse_git_log_multiple() {
        let output = format!(
            "{sep}aaa{fs}Alice{fs}2024-01-01{fs}First{fs}{fs}\
             {sep}bbb{fs}Bob{fs}2024-01-02{fs}Second{fs}body{fs}",
            sep = COMMIT_SEP,
            fs = FIELD_SEP,
        );
        let commits = parse_git_log(&output);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].hash, "aaa");
        assert_eq!(commits[1].hash, "bbb");
    }

    #[test]
    fn test_parse_blame_porcelain() {
        let blame = "\
abc123def4567890123456789012345678901234 1 1 3\n\
author Alice\n\
author-mail <alice@example.com>\n\
author-time 1700000000\n\
author-tz +0000\n\
committer Alice\n\
committer-mail <alice@example.com>\n\
committer-time 1700000000\n\
committer-tz +0000\n\
summary Fix something\n\
filename src/main.rs\n\
\tlet x = 1;\n\
abc123def4567890123456789012345678901234 1 2\n\
author Alice\n\
author-time 1700000000\n\
\tlet y = 2;\n";

        let entries = parse_blame_porcelain(blame);
        // Should merge into one entry since same commit and consecutive lines
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].start_line, 1);
        assert!(entries[0].end_line >= 2);
        assert_eq!(entries[0].author, "Alice");
    }

    #[test]
    fn test_is_git_history_enabled_default() {
        // Non-existent path should return false
        assert!(!is_git_history_enabled(Path::new("/nonexistent")));
    }
}
