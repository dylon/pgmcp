//! Codex-specific JSONL parsers for session transcripts and prompt history.
//!
//! Codex stores prompt history at `~/.codex/history.jsonl` and session
//! rollouts under `~/.codex/sessions/YYYY/MM/DD/*.jsonl`. The session files
//! contain a mix of durable response items, UI events, metadata, reasoning,
//! and tool I/O; this parser keeps the semantically useful user/assistant/tool
//! text and skips private/noisy records.

use std::path::Path;

use serde_json::Value;

use super::chunker::Chunk;

const MAX_TOOL_TEXT_LEN: usize = 10_000;

/// Parse a Codex history or session JSONL file into semantic chunks.
pub fn chunk_codex_jsonl(content: &str) -> Vec<Chunk> {
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

        let text = extract_entry_text(&entry);
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

fn extract_entry_text(entry: &Value) -> String {
    // `~/.codex/history.jsonl` entries are compact prompt-history records:
    // {"session_id":"...","ts":...,"text":"..."}
    if let Some(text) = entry.get("text").and_then(Value::as_str)
        && entry.get("session_id").is_some()
    {
        return format!("[history] {}", text);
    }

    let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("");
    if entry_type != "response_item" {
        return String::new();
    }

    let payload = match entry.get("payload").and_then(Value::as_object) {
        Some(payload) => payload,
        None => return String::new(),
    };

    match payload.get("type").and_then(Value::as_str).unwrap_or("") {
        "message" => {
            let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
            let prefix = match role {
                "user" => "[user]",
                "assistant" => "[assistant]",
                _ => return String::new(),
            };
            let text = payload
                .get("content")
                .map(extract_content_text)
                .unwrap_or_default();
            if text.is_empty() {
                String::new()
            } else {
                format!("{} {}", prefix, text)
            }
        }
        "function_call" => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let arguments = payload
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if arguments.is_empty() || arguments.len() > MAX_TOOL_TEXT_LEN {
                String::new()
            } else {
                format!("[tool_call:{}] {}", name, arguments)
            }
        }
        "function_call_output" => {
            let output = payload
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if output.is_empty() || output.len() > MAX_TOOL_TEXT_LEN {
                String::new()
            } else {
                format!("[tool_output] {}", output)
            }
        }
        _ => String::new(),
    }
}

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

/// Check if a path is a Codex prompt-history or session transcript JSONL.
pub fn is_codex_jsonl(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    path_str.ends_with("/.codex/history.jsonl")
        || (path_str.contains("/.codex/sessions/") && path_str.ends_with(".jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chunk_codex_history_prompt() {
        let jsonl = r#"{"session_id":"s","ts":1710000000,"text":"add tests for parser"}"#;
        let chunks = chunk_codex_jsonl(jsonl);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.starts_with("[history]"));
        assert!(chunks[0].content.contains("add tests"));
    }

    #[test]
    fn chunk_codex_session_user_and_assistant_messages() {
        let jsonl = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}"#,
            "\n",
        );
        let chunks = chunk_codex_jsonl(jsonl);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].content.starts_with("[user]"));
        assert!(chunks[1].content.starts_with("[assistant]"));
    }

    #[test]
    fn chunk_codex_tool_call_and_output() {
        let jsonl = concat!(
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"rg parser\"}","call_id":"c"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call_output","output":"src/indexer/codex_chunker.rs","call_id":"c"}}"#,
            "\n",
        );
        let chunks = chunk_codex_jsonl(jsonl);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].content.starts_with("[tool_call:exec_command]"));
        assert!(chunks[1].content.starts_with("[tool_output]"));
    }

    #[test]
    fn chunk_codex_skips_private_and_noisy_records() {
        let jsonl = concat!(
            r#"{"type":"session_meta","payload":{"cwd":"/workspace","model":"gpt"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"text":"secret instruction"}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"reasoning","summary":[]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call_output","encrypted_content":"abc"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{}}}"#,
            "\n",
            "not json",
            "\n",
        );
        assert!(chunk_codex_jsonl(jsonl).is_empty());
    }

    #[test]
    fn chunk_codex_skips_oversized_tool_output() {
        let output = "x".repeat(MAX_TOOL_TEXT_LEN + 1);
        let jsonl = json!({
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "output": output
            }
        })
        .to_string();
        assert!(chunk_codex_jsonl(&jsonl).is_empty());
    }

    #[test]
    fn chunk_codex_indices_are_dense_and_lines_are_preserved() {
        let jsonl = concat!(
            "not json\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"text":"first"}]}}"#,
            "\n\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"text":"second"}]}}"#,
            "\n",
        );
        let chunks = chunk_codex_jsonl(jsonl);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chunk_index, 0);
        assert_eq!(chunks[0].start_line, 2);
        assert_eq!(chunks[1].chunk_index, 1);
        assert_eq!(chunks[1].start_line, 4);
    }

    #[test]
    fn detects_codex_jsonl_paths() {
        assert!(is_codex_jsonl(Path::new("/home/user/.codex/history.jsonl")));
        assert!(is_codex_jsonl(Path::new(
            "/home/user/.codex/sessions/2026/05/12/rollout.jsonl"
        )));
        assert!(!is_codex_jsonl(Path::new(
            "/home/user/.codex/cache/noise.jsonl"
        )));
        assert!(!is_codex_jsonl(Path::new("/home/user/project/data.jsonl")));
    }
}
