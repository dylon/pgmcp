//! Memory-server Phase 4: extraction + reflection prompt templates.
//!
//! Versioned so a future template change can be detected via the
//! `model_signature` (which encodes prompt-template version too).
//!
//! The extraction prompt asks for JSON matching `ExtractionResult`'s
//! schema. Implementations should reject responses that don't parse
//! against that schema rather than try to "fix" partial output —
//! garbage-in to `memory_*` is worse than no-extraction.

use crate::llm::{EntityRef, ExtractionRequest};

/// Schema version stamped onto extracted observations. Bump whenever
/// the prompt or output shape changes in a way that would make older
/// extractions inconsistent.
pub const EXTRACTION_PROMPT_VERSION: &str = "v1";

/// Construct the extraction prompt for one `ExtractionRequest`. Output
/// is a fully-formed user-turn string suitable for whichever backend's
/// chat-template formatter (Anthropic JSON request, Qwen3 chat
/// template) the caller chooses.
pub fn build_extraction_prompt(req: &ExtractionRequest<'_>) -> String {
    let mut s = String::with_capacity(2048);
    s.push_str(EXTRACTION_SYSTEM);
    s.push_str("\n\n");
    s.push_str("USER PROMPT:\n");
    s.push_str(req.text);
    s.push_str("\n\n");
    if !req.existing_entities.is_empty() {
        s.push_str(
            "EXISTING ENTITIES (use exact `name` strings to attach new facts to these — do not invent variants):\n",
        );
        for e in req.existing_entities {
            push_entity_summary(&mut s, e);
        }
        s.push('\n');
    }
    s.push_str("Return ONLY a JSON object that parses against this schema:\n");
    s.push_str(SCHEMA);
    s.push_str("\n\nDo not include backticks, prose, or commentary — JSON only.\n");
    s
}

/// Reflection prompt. Asks for higher-order observations grounded in a
/// list of recent observation contents.
pub fn build_reflection_prompt(observations: &[String]) -> String {
    let mut s = String::with_capacity(2048);
    s.push_str(REFLECTION_SYSTEM);
    s.push_str("\n\nRECENT OBSERVATIONS:\n");
    for (i, o) in observations.iter().enumerate() {
        s.push_str(&format!("[{}] {}\n", i + 1, o));
    }
    s.push_str("\nReturn ONLY a JSON array of entities matching:\n");
    s.push_str(REFLECTION_SCHEMA);
    s.push_str("\n\nDo not include backticks, prose, or commentary — JSON only.\n");
    s
}

fn push_entity_summary(s: &mut String, e: &EntityRef) {
    s.push_str(&format!("- name: {}\n  type: {}\n", e.name, e.entity_type));
    if !e.key_observations.is_empty() {
        s.push_str("  recent observations:\n");
        for o in &e.key_observations {
            s.push_str(&format!("    - {}\n", o));
        }
    }
}

pub const EXTRACTION_SYSTEM: &str = concat!(
    "You extract structured, durable facts from a user prompt addressed ",
    "to a coding agent. Only emit facts the user actually states or that ",
    "are clearly implied by an imperative ('always do X', 'never use Y'). ",
    "Do not invent. Mark contradictions explicitly via the `contradictions` ",
    "array when the new prompt invalidates an existing entity/observation/relation. ",
    "Importance is a value in [0,1]: 0.9+ = explicit standing rule, 0.5–0.8 = ",
    "factual preference, < 0.3 = transient / weakly-grounded. Reject the whole ",
    "extraction if you cannot confidently produce JSON matching the schema."
);

pub const REFLECTION_SYSTEM: &str = concat!(
    "You are a reflection step over a user's recent memory. Identify higher-order ",
    "patterns, durable preferences, or summaries worth remembering. Cite specific ",
    "observation numbers (the [N] indices) in `initial_observations[0]` when ",
    "summarizing. Emit at most 5 entities; importance must be ≥ 0.5 (reflection-emitted ",
    "facts that don't pass that bar should not be emitted at all)."
);

/// Schemars-derived schema string for `ExtractionResult`. Kept literal
/// (not generated at runtime) so the prompt is deterministic; if the
/// `ExtractionResult` shape changes, regenerate this constant.
pub const SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "entities": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["name", "entity_type"],
        "properties": {
          "name": { "type": "string" },
          "entity_type": { "type": "string" },
          "initial_observations": { "type": "array", "items": { "type": "string" } },
          "importance": { "type": "number", "minimum": 0, "maximum": 1 }
        }
      }
    },
    "relations": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["from_name", "to_name", "relation_type"],
        "properties": {
          "from_name": { "type": "string" },
          "to_name": { "type": "string" },
          "relation_type": { "type": "string" },
          "importance": { "type": "number", "minimum": 0, "maximum": 1 }
        }
      }
    },
    "contradictions": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["conflicting_with", "kind", "reason"],
        "properties": {
          "conflicting_with": { "type": "integer" },
          "kind": { "type": "string", "enum": ["observation", "relation"] },
          "reason": { "type": "string" }
        }
      }
    }
  }
}"#;

pub const REFLECTION_SCHEMA: &str = r#"[
  {
    "type": "object",
    "required": ["name", "entity_type"],
    "properties": {
      "name": { "type": "string" },
      "entity_type": { "type": "string" },
      "initial_observations": { "type": "array", "items": { "type": "string" } },
      "importance": { "type": "number", "minimum": 0, "maximum": 1 }
    }
  }
]"#;

/// Strip a fenced code block from a model response that ignored the
/// "JSON only" instruction. Strips ```json ... ``` and ``` ... ```.
/// Returns the inner text or the original on failure.
pub fn strip_code_fences(text: &str) -> &str {
    let t = text.trim();
    if let Some(rest) = t.strip_prefix("```json")
        && let Some(inner) = rest.strip_suffix("```")
    {
        return inner.trim();
    }
    if let Some(rest) = t.strip_prefix("```")
        && let Some(inner) = rest.strip_suffix("```")
    {
        return inner.trim();
    }
    t
}

/// Extract the first balanced JSON object/array substring from a
/// model response. Useful when the model emits a preamble before the
/// JSON despite the "JSON only" instruction.
pub fn extract_first_json(text: &str) -> Option<&str> {
    let s = text;
    let bytes = s.as_bytes();
    let mut start: Option<usize> = None;
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut esc = false;
    let mut open_char = b'{';
    for (i, b) in bytes.iter().copied().enumerate() {
        if start.is_none() {
            if b == b'{' || b == b'[' {
                start = Some(i);
                open_char = b;
                depth = 1;
            }
            continue;
        }
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    let begin = start.unwrap();
                    // Sanity: closing char must match.
                    let close_match =
                        (open_char == b'{' && b == b'}') || (open_char == b'[' && b == b']');
                    if !close_match {
                        return None;
                    }
                    return Some(&s[begin..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_code_fences_handles_json_block() {
        let raw = "```json\n{\"x\": 1}\n```";
        assert_eq!(strip_code_fences(raw), "{\"x\": 1}");
    }

    #[test]
    fn strip_code_fences_handles_unmarked_block() {
        let raw = "```\n{\"x\": 1}\n```";
        assert_eq!(strip_code_fences(raw), "{\"x\": 1}");
    }

    #[test]
    fn strip_code_fences_passthrough() {
        assert_eq!(strip_code_fences("{\"x\":1}"), "{\"x\":1}");
    }

    #[test]
    fn extract_first_json_object() {
        let raw = "Sure, here's the JSON:\n{\"a\":1,\"b\":[2,3]}\nLet me know.";
        assert_eq!(extract_first_json(raw), Some("{\"a\":1,\"b\":[2,3]}"));
    }

    #[test]
    fn extract_first_json_array() {
        let raw = "Output: [{\"a\":1},{\"b\":2}] trailing prose";
        assert_eq!(extract_first_json(raw), Some("[{\"a\":1},{\"b\":2}]"));
    }

    #[test]
    fn extract_first_json_respects_strings() {
        // `}` inside a string must not close the outer object.
        let raw = "{\"k\":\"value with } and ] in it\",\"x\":1}";
        assert_eq!(
            extract_first_json(raw),
            Some("{\"k\":\"value with } and ] in it\",\"x\":1}")
        );
    }

    #[test]
    fn extract_first_json_none_when_unbalanced() {
        assert_eq!(extract_first_json("{ no close"), None);
    }
}
