//! Claude-specific JSONL parsers for session transcripts and file-history metadata.
//!
//! Session transcripts (`projects/*/*.jsonl`) contain user/assistant/tool messages.
//! File-history directories (`file-history/<session-uuid>/`) contain before/after
//! file snapshots that can be cross-referenced back to original paths via the JSONL.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use super::chunker::Chunk;

/// Known message types in Claude session transcripts that we skip.
const SKIP_TYPES: &[&str] = &[
    "progress",
    "queue-operation",
    "file-history-snapshot",
];

/// Parse a Claude session transcript JSONL file into chunks.
/// Each user/assistant message becomes one chunk.
/// Tool results with text content are included; binary/large outputs are skipped.
pub fn chunk_claude_jsonl(content: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut chunk_index: i32 = 0;

    for (line_num, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let entry: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Check if this is a type we should skip
        let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("");
        if SKIP_TYPES.iter().any(|&t| t == entry_type) {
            continue;
        }

        let text = extract_message_text(&entry);
        if text.is_empty() {
            continue;
        }

        let line_1based = (line_num + 1) as i32;
        chunks.push(Chunk {
            chunk_index,
            content: text,
            start_line: line_1based,
            end_line: line_1based,
        });
        chunk_index += 1;
    }

    chunks
}

/// Extract displayable text from a Claude session transcript entry.
fn extract_message_text(entry: &Value) -> String {
    let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("");

    match entry_type {
        "user" | "human" => {
            let prefix = "[user]";
            if let Some(text) = entry.get("message").and_then(Value::as_str) {
                return format!("{} {}", prefix, text);
            }
            if let Some(content) = entry.get("content") {
                return format!("{} {}", prefix, extract_content_text(content));
            }
            String::new()
        }
        "assistant" => {
            let prefix = "[assistant]";
            if let Some(text) = entry.get("message").and_then(Value::as_str) {
                return format!("{} {}", prefix, text);
            }
            if let Some(content) = entry.get("content") {
                return format!("{} {}", prefix, extract_content_text(content));
            }
            String::new()
        }
        "tool_result" | "tool_use" => {
            let tool_name = entry
                .get("name")
                .or_else(|| entry.get("tool_name"))
                .and_then(Value::as_str)
                .unwrap_or("tool");

            if let Some(content) = entry.get("content") {
                let text = extract_content_text(content);
                if !text.is_empty() && text.len() < 10_000 {
                    return format!("[{}] {}", tool_name, text);
                }
            }
            if let Some(result) = entry.get("result").and_then(Value::as_str) {
                if result.len() < 10_000 {
                    return format!("[{}] {}", tool_name, result);
                }
            }
            String::new()
        }
        _ => {
            // Generic fallback: try "message" or "content" fields
            if let Some(text) = entry.get("message").and_then(Value::as_str) {
                return text.to_string();
            }
            if let Some(content) = entry.get("content") {
                let text = extract_content_text(content);
                if !text.is_empty() {
                    return text;
                }
            }
            String::new()
        }
    }
}

/// Extract text from a content field which may be a string or an array of content blocks.
fn extract_content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    parts.push(text);
                } else if let Some(text) = block.as_str() {
                    parts.push(text);
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

/// Metadata about a file-history snapshot entry.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FileHistoryEntry {
    /// The original file path that was edited.
    pub original_path: String,
    /// Version number (1 = before edit, 2 = after edit).
    pub version: u32,
    /// The backup file name (hash-based).
    pub backup_filename: String,
    /// Session ID this edit belongs to.
    pub session_id: String,
}

/// Parse file-history metadata from a Claude session JSONL, building a map from
/// `backupFileName` → `FileHistoryEntry`.
///
/// The JSONL contains entries like:
/// ```json
/// {"type": "file-history-snapshot", "filePath": "/path/to/file.rs",
///  "backupFileName": "abc123@v1", "sessionId": "..."}
/// ```
#[allow(dead_code)]
pub fn parse_file_history_map(content: &str) -> HashMap<String, FileHistoryEntry> {
    let mut map = HashMap::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let entry: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("");
        if entry_type != "file-history-snapshot" {
            continue;
        }

        let original_path = match entry.get("filePath").and_then(Value::as_str) {
            Some(p) => p.to_string(),
            None => continue,
        };
        let backup_filename = match entry.get("backupFileName").and_then(Value::as_str) {
            Some(f) => f.to_string(),
            None => continue,
        };
        let session_id = entry
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        // Parse version from backup filename: "hash@v1" or "hash@v2"
        let version = backup_filename
            .rsplit_once("@v")
            .and_then(|(_, v)| v.parse::<u32>().ok())
            .unwrap_or(1);

        map.insert(
            backup_filename.clone(),
            FileHistoryEntry {
                original_path,
                version,
                backup_filename,
                session_id,
            },
        );
    }

    map
}

/// Check if a path is a Claude session transcript (lives in `projects/*/` under `~/.claude/`).
pub fn is_claude_session_transcript(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    // Pattern: ~/.claude/projects/<project-hash>/<session-id>.jsonl
    path_str.contains("/.claude/projects/") && path_str.ends_with(".jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_claude_jsonl_user_message() {
        let jsonl = r#"{"type": "user", "message": "How do I fix this bug?"}"#;
        let chunks = chunk_claude_jsonl(jsonl);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.starts_with("[user]"));
        assert!(chunks[0].content.contains("fix this bug"));
    }

    #[test]
    fn test_chunk_claude_jsonl_assistant_message() {
        let jsonl = r#"{"type": "assistant", "message": "Here is the fix..."}"#;
        let chunks = chunk_claude_jsonl(jsonl);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.starts_with("[assistant]"));
    }

    #[test]
    fn test_chunk_claude_jsonl_skips_progress() {
        let jsonl = r#"{"type": "progress", "data": "..."}"#;
        let chunks = chunk_claude_jsonl(jsonl);
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_chunk_claude_jsonl_skips_file_history_snapshot() {
        let jsonl = r#"{"type": "file-history-snapshot", "filePath": "/foo.rs", "backupFileName": "abc@v1"}"#;
        let chunks = chunk_claude_jsonl(jsonl);
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_chunk_claude_jsonl_tool_result() {
        let jsonl = r#"{"type": "tool_result", "name": "Read", "result": "file content here"}"#;
        let chunks = chunk_claude_jsonl(jsonl);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.starts_with("[Read]"));
    }

    #[test]
    fn test_chunk_claude_jsonl_content_array() {
        let jsonl = r#"{"type": "assistant", "content": [{"text": "part1"}, {"text": "part2"}]}"#;
        let chunks = chunk_claude_jsonl(jsonl);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.contains("part1"));
        assert!(chunks[0].content.contains("part2"));
    }

    #[test]
    fn test_chunk_claude_jsonl_multiple_messages() {
        let jsonl = concat!(
            r#"{"type": "user", "message": "question 1"}"#, "\n",
            r#"{"type": "assistant", "message": "answer 1"}"#, "\n",
            r#"{"type": "user", "message": "question 2"}"#, "\n",
        );
        let chunks = chunk_claude_jsonl(jsonl);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].chunk_index, 0);
        assert_eq!(chunks[1].chunk_index, 1);
        assert_eq!(chunks[2].chunk_index, 2);
    }

    #[test]
    fn test_parse_file_history_map() {
        let jsonl = concat!(
            r#"{"type": "file-history-snapshot", "filePath": "/home/user/project/main.rs", "backupFileName": "abc123@v1", "sessionId": "sess-1"}"#, "\n",
            r#"{"type": "file-history-snapshot", "filePath": "/home/user/project/main.rs", "backupFileName": "abc123@v2", "sessionId": "sess-1"}"#, "\n",
            r#"{"type": "user", "message": "edit main.rs"}"#, "\n",
        );
        let map = parse_file_history_map(jsonl);
        assert_eq!(map.len(), 2);

        let v1 = &map["abc123@v1"];
        assert_eq!(v1.original_path, "/home/user/project/main.rs");
        assert_eq!(v1.version, 1);

        let v2 = &map["abc123@v2"];
        assert_eq!(v2.version, 2);
    }

    #[test]
    fn test_is_claude_session_transcript() {
        assert!(is_claude_session_transcript(Path::new(
            "/home/user/.claude/projects/-home-user-myproject/abc123.jsonl"
        )));
        assert!(!is_claude_session_transcript(Path::new(
            "/home/user/.claude/CLAUDE.md"
        )));
        assert!(!is_claude_session_transcript(Path::new(
            "/home/user/project/data.jsonl"
        )));
    }

    #[test]
    fn test_empty_input() {
        assert!(chunk_claude_jsonl("").is_empty());
        assert!(chunk_claude_jsonl("  \n  \n  ").is_empty());
    }

    #[test]
    fn test_invalid_json_lines_skipped() {
        let jsonl = concat!(
            "not json\n",
            r#"{"type": "user", "message": "valid"}"#, "\n",
            "also not json\n",
        );
        let chunks = chunk_claude_jsonl(jsonl);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn test_large_tool_result_skipped() {
        let large_content = "x".repeat(15_000);
        let jsonl = format!(
            r#"{{"type": "tool_result", "name": "Read", "result": "{}"}}"#,
            large_content
        );
        let chunks = chunk_claude_jsonl(&jsonl);
        assert!(chunks.is_empty());
    }
}
