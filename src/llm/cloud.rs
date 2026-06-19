//! Memory-server Phase 4: cloud (Anthropic) `LlmExtractor` backend.
//!
//! Opt-in path for users who don't want to download Qwen3 weights or
//! who want stronger extraction quality at the cost of an API call.
//! Reads `ANTHROPIC_API_KEY` from the environment; refuses to construct
//! if the key is missing (loud failure beats silent no-op).
//!
//! Targets the `anthropic-version: 2023-06-01` Messages API. Uses
//! `claude-haiku-4-5` by default — small + cheap + plenty for
//! salience extraction.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use tracing::{debug, error};

use crate::llm::prompt::{
    build_extraction_prompt, build_reflection_prompt, extract_first_json, strip_code_fences,
};
use crate::llm::{ExtractionRequest, ExtractionResult, LlmExtractor, NewEntity};

const MODEL: &str = "claude-haiku-4-5";
const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";

/// Anthropic-API-backed extractor. Cheap, sturdy, gated on an API key.
pub struct AnthropicExtractor {
    api_key: String,
    client: ureq::Agent,
}

impl AnthropicExtractor {
    pub fn new() -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
            anyhow!(
                "AnthropicExtractor: ANTHROPIC_API_KEY env var not set. Set it before \
                 selecting [memory.extractor] backend = \"cloud\", or pick a local backend."
            )
        })?;
        let client = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(60))
            .build();
        Ok(Self { api_key, client })
    }

    fn invoke(&self, prompt: String, max_tokens: usize) -> Result<String> {
        let body = serde_json::json!({
            "model": MODEL,
            "max_tokens": max_tokens,
            "messages": [
                {"role": "user", "content": prompt}
            ],
        });
        let resp = self
            .client
            .post(API_URL)
            .set("x-api-key", &self.api_key)
            .set("anthropic-version", API_VERSION)
            .set("content-type", "application/json")
            .send_string(&serde_json::to_string(&body)?);
        let resp = match resp {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_else(|_| "<unreadable>".into());
                return Err(anyhow!("Anthropic API HTTP {}: {}", code, body));
            }
            Err(e) => return Err(anyhow!("Anthropic API transport error: {}", e)),
        };
        let text = resp.into_string().context("read Anthropic response body")?;
        let parsed: AnthropicMessagesResponse =
            serde_json::from_str(&text).context("parse Anthropic response")?;
        // Concatenate all text-content blocks.
        let mut out = String::new();
        for block in parsed.content {
            if block.kind == "text" {
                out.push_str(&block.text);
            }
        }
        debug!(
            response_len = out.len(),
            stop_reason = ?parsed.stop_reason,
            "anthropic extractor: completion",
        );
        Ok(out)
    }
}

#[derive(Debug, Deserialize)]
struct AnthropicMessagesResponse {
    #[serde(default)]
    content: Vec<AnthropicContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

impl LlmExtractor for AnthropicExtractor {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn model_signature(&self) -> &'static str {
        signature_for("anthropic-haiku-4-5-extractor-v1")
    }

    fn extract(&self, request: ExtractionRequest<'_>) -> Result<ExtractionResult> {
        let prompt = build_extraction_prompt(&request);
        let raw = self.invoke(prompt, 2048)?;
        parse_extraction_response(&raw)
    }

    fn reflect(&self, observations: &[String]) -> Result<Vec<NewEntity>> {
        if observations.is_empty() {
            return Ok(Vec::new());
        }
        let prompt = build_reflection_prompt(observations);
        let raw = self.invoke(prompt, 2048)?;
        parse_reflection_response(&raw)
    }
}

/// Shared response parser for the extraction path. Tries
/// (a) raw parse,
/// (b) parse-after-code-fence-strip,
/// (c) parse-after-first-balanced-JSON extraction.
/// Rejects (returns Err) if all three fail — we never partially trust
/// the LLM's output.
pub fn parse_extraction_response(raw: &str) -> Result<ExtractionResult> {
    if let Ok(r) = serde_json::from_str::<ExtractionResult>(raw.trim()) {
        return Ok(r);
    }
    let stripped = strip_code_fences(raw);
    if let Ok(r) = serde_json::from_str::<ExtractionResult>(stripped) {
        return Ok(r);
    }
    if let Some(json) = extract_first_json(raw)
        && let Ok(r) = serde_json::from_str::<ExtractionResult>(json)
    {
        return Ok(r);
    }
    error!(
        len = raw.len(),
        head = %&raw.chars().take(200).collect::<String>(),
        "extractor response could not be parsed as ExtractionResult"
    );
    Err(anyhow!(
        "extractor response failed JSON validation against ExtractionResult schema"
    ))
}

pub fn parse_reflection_response(raw: &str) -> Result<Vec<NewEntity>> {
    if let Ok(r) = serde_json::from_str::<Vec<NewEntity>>(raw.trim()) {
        return Ok(r);
    }
    let stripped = strip_code_fences(raw);
    if let Ok(r) = serde_json::from_str::<Vec<NewEntity>>(stripped) {
        return Ok(r);
    }
    if let Some(json) = extract_first_json(raw)
        && let Ok(r) = serde_json::from_str::<Vec<NewEntity>>(json)
    {
        return Ok(r);
    }
    Err(anyhow!(
        "reflection response failed JSON validation against Vec<NewEntity>"
    ))
}

/// Per-process signature cache so `model_signature(&self)` can return
/// a `&'static str` (interned via OnceLock).
fn signature_for(label: &'static str) -> &'static str {
    static SLOT: OnceLock<&'static str> = OnceLock::new();
    SLOT.get_or_init(|| label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extraction_response_raw_json() {
        let raw = r#"{"entities":[{"name":"x","entity_type":"y","importance":0.6}]}"#;
        let r = parse_extraction_response(raw).unwrap();
        assert_eq!(r.entities.len(), 1);
        assert_eq!(r.entities[0].name, "x");
    }

    #[test]
    fn parse_extraction_response_with_fence() {
        let raw = "```json\n{\"entities\":[],\"relations\":[]}\n```";
        let r = parse_extraction_response(raw).unwrap();
        assert!(r.entities.is_empty());
        assert!(r.relations.is_empty());
    }

    #[test]
    fn parse_extraction_response_with_preamble() {
        let raw = "Sure! Here's the structured output:\n\n{\"entities\":[{\"name\":\"rust\",\"entity_type\":\"language\"}]}\nLet me know if you need more.";
        let r = parse_extraction_response(raw).unwrap();
        assert_eq!(r.entities.len(), 1);
        assert_eq!(r.entities[0].name, "rust");
    }

    #[test]
    fn parse_extraction_response_rejects_garbage() {
        let raw = "I cannot comply with that request.";
        assert!(parse_extraction_response(raw).is_err());
    }

    #[test]
    fn parse_reflection_response_array() {
        let raw = "[{\"name\":\"pattern1\",\"entity_type\":\"summary\",\"importance\":0.7}]";
        let r = parse_reflection_response(raw).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "pattern1");
    }

    #[test]
    fn parse_reflection_response_rejects_object() {
        // Reflection schema is an array; bare object should fail.
        let raw = "{\"name\":\"x\",\"entity_type\":\"y\"}";
        assert!(parse_reflection_response(raw).is_err());
    }
}
