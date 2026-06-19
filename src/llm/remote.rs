//! Memory-server: remote OpenAI-compatible `LlmExtractor` backend (Crucible E1).
//!
//! Lets the **daemon itself** call a network-reachable OpenAI-compatible chat
//! endpoint (e.g. the DGX Spark `sparky`: ollama on `:11434/v1`, or the DeepSeek-V4
//! server on `:8000` reached over an SSH tunnel) for salience extraction and
//! reflection — moving that work *off* the contended 8 GiB local GPU, which the
//! BGE-M3 embedder pool occupies.
//!
//! Mirrors `cloud.rs`: synchronous `ureq` transport (the `LlmExtractor` trait is
//! sync; the worker wraps each call in `spawn_blocking`), env-gated construction
//! (loud failure beats a silent no-op), and reuse of the shared prompt builders +
//! robust response parsers in `crate::llm::{prompt, cloud}`.
//!
//! Configuration is by environment (set by the daemon from the `[llm]` config
//! section, or directly): `PGMCP_LLM_BASE_URL` (required, e.g.
//! `http://localhost:8001/v1`), `PGMCP_LLM_MODEL` (required, e.g.
//! `deepseek-v4-flash`), `PGMCP_LLM_API_KEY` (optional — local servers ignore it).
//! Selected via `[memory.extractor] backend = "remote-openai"`.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use tracing::{debug, error};

use crate::llm::cloud::{parse_extraction_response, parse_reflection_response};
use crate::llm::prompt::{build_extraction_prompt, build_reflection_prompt};
use crate::llm::{ExtractionRequest, ExtractionResult, LlmExtractor, NewEntity};

const DEFAULT_TIMEOUT_SECS: u64 = 180;

/// OpenAI-compatible chat-completions extractor. Stateless per call; gated on a
/// reachable `base_url` + `model`.
pub struct RemoteOpenAiExtractor {
    base_url: String,
    model: String,
    api_key: String,
    client: ureq::Agent,
    /// Interned provenance signature, e.g. `"remote-openai/deepseek-v4-flash-v1"`.
    signature: &'static str,
}

impl RemoteOpenAiExtractor {
    /// Construct from explicit values.
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        let base_url = base_url.into();
        let model = model.into();
        let timeout = std::env::var("PGMCP_LLM_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        let client = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(timeout))
            .build();
        // Intern a &'static provenance signature (one small, bounded leak per
        // long-lived extractor singleton — the &'static the trait requires).
        let signature: &'static str =
            Box::leak(format!("remote-openai/{}-v1", model).into_boxed_str());
        Self {
            base_url,
            model,
            api_key: api_key.into(),
            client,
            signature,
        }
    }

    /// Construct from the environment. Refuses (Err) if `PGMCP_LLM_BASE_URL` or
    /// `PGMCP_LLM_MODEL` is unset, so a misconfigured `backend = "remote-openai"`
    /// fails loudly at startup rather than silently no-op'ing.
    pub fn from_env() -> Result<Self> {
        let base_url = std::env::var("PGMCP_LLM_BASE_URL").map_err(|_| {
            anyhow!(
                "RemoteOpenAiExtractor: PGMCP_LLM_BASE_URL not set. Set it (e.g. \
                 http://localhost:8001/v1 for sparky DeepSeek over the tunnel) before \
                 selecting [memory.extractor] backend = \"remote-openai\", or pick another backend."
            )
        })?;
        let model = std::env::var("PGMCP_LLM_MODEL").map_err(|_| {
            anyhow!("RemoteOpenAiExtractor: PGMCP_LLM_MODEL not set (e.g. deepseek-v4-flash, qwen3:14b).")
        })?;
        // Local OpenAI-compatible servers (ollama / llama-server / the DeepSeek
        // stack) ignore the key; default to a non-empty placeholder.
        let api_key = std::env::var("PGMCP_LLM_API_KEY").unwrap_or_else(|_| "pgmcp".to_string());
        Ok(Self::new(base_url, model, api_key))
    }

    /// One chat-completions round trip; returns the assistant message content.
    fn invoke(&self, prompt: String, max_tokens: usize) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "temperature": 0,
            "stream": false,
            "messages": [ { "role": "user", "content": prompt } ],
        });
        let resp = self
            .client
            .post(&url)
            .set("authorization", &format!("Bearer {}", self.api_key))
            .set("content-type", "application/json")
            .send_string(&serde_json::to_string(&body)?);
        let resp = match resp {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_else(|_| "<unreadable>".into());
                return Err(anyhow!("remote OpenAI HTTP {} at {}: {}", code, url, body));
            }
            Err(e) => return Err(anyhow!("remote OpenAI transport error at {}: {}", url, e)),
        };
        let text = resp
            .into_string()
            .context("read remote OpenAI response body")?;
        let parsed: ChatCompletionResponse = serde_json::from_str(&text).with_context(|| {
            format!(
                "parse remote OpenAI response: {}",
                &text.chars().take(300).collect::<String>()
            )
        })?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| anyhow!("remote OpenAI response had no choices"))?;
        debug!(response_len = content.len(), model = %self.model, "remote extractor: completion");
        Ok(content)
    }
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    #[serde(default)]
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    #[serde(default)]
    content: String,
}

impl LlmExtractor for RemoteOpenAiExtractor {
    fn name(&self) -> &'static str {
        "remote-openai"
    }

    fn model_signature(&self) -> &'static str {
        self.signature
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
        match parse_reflection_response(&raw) {
            Ok(v) => Ok(v),
            Err(e) => {
                error!(error = %e, "remote extractor: reflection parse failed");
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_includes_model() {
        let x = RemoteOpenAiExtractor::new("http://localhost:8001/v1", "deepseek-v4-flash", "k");
        assert_eq!(x.name(), "remote-openai");
        assert_eq!(x.model_signature(), "remote-openai/deepseek-v4-flash-v1");
    }

    #[test]
    fn chat_response_parses() {
        let j = r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}]}"#;
        let r: ChatCompletionResponse = serde_json::from_str(j).unwrap();
        assert_eq!(r.choices.len(), 1);
        assert_eq!(r.choices[0].message.content, "hello");
    }
}
