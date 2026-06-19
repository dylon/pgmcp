//! LLM-as-judge relevance grading for the conceptual-query evaluation (Epic 2).
//!
//! Conceptual queries have no single pre-authored gold (see
//! [`crate::eval::query::ConceptualQuery`]): instead, the candidates **pooled**
//! across `semantic`/`hybrid`/`text` are graded 0–3 by a local LLM, and those
//! grades become the graded gold the rank metrics score against. A second,
//! cross-family judge grades a sample so we can report inter-judge agreement
//! (quadratic-weighted Cohen's κ, [`crate::eval::stats::cohens_kappa_quadratic`]).
//!
//! Both judges run **locally on `sparky`** (the DGX Spark) behind an
//! OpenAI-compatible chat API — qwen3-32B via ollama (`http://sparky:11434/v1`)
//! as primary, DeepSeek-V4 via the `:8000` server (reached over an SSH tunnel)
//! as the κ cross-check. The judge sees only `(query, passage)` — never which
//! search system produced the candidate — so its grades cannot favour a mode.

use anyhow::{Context, Result};
use serde::Deserialize;

/// An OpenAI-compatible chat client for one LLM judge. Stateless beyond the
/// endpoint + model; deterministic (temperature 0).
pub struct JudgeClient {
    base_url: String,
    model: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}
#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}
#[derive(Deserialize)]
struct ChatMessage {
    #[serde(default)]
    content: String,
}

impl JudgeClient {
    /// `base_url` is the OpenAI-compatible root (…ending in `/v1`); `model` is
    /// the served model id; `api_key` is optional (local servers need none).
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .build()
            .context("build judge HTTP client")?;
        Ok(Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key,
            client,
        })
    }

    /// Model signature — used for grade-cache keys, κ arm labels, and ledger
    /// provenance.
    pub fn signature(&self) -> &str {
        &self.model
    }

    /// A liveness/availability probe: a trivial grade. Returns `Ok(())` if the
    /// endpoint answers with a parseable grade, so the campaign can fail fast
    /// (and skip the arm) rather than erroring on every candidate.
    pub async fn smoke(&self) -> Result<u8> {
        self.grade(
            "how are errors handled",
            "src/example.rs",
            "fn handle(e: Error) -> Result<()> { log::error!(\"{e}\"); Err(e) }",
        )
        .await
    }

    /// Grade one `(query, passage)` for relevance on the 0–3 rubric.
    /// Deterministic; returns an error on transport/HTTP/parse failure so the
    /// caller can skip the pair rather than silently scoring it 0.
    pub async fn grade(&self, query: &str, path: &str, passage: &str) -> Result<u8> {
        let prompt = build_rubric_prompt(query, path, passage);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": prompt}],
            "temperature": 0,
            "max_tokens": 600,
            "stream": false,
        });
        let mut req = self.client.post(&url).json(&body);
        if let Some(k) = &self.api_key {
            req = req.bearer_auth(k);
        }
        let resp = req.send().await.context("judge request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("read judge body")?;
        if !status.is_success() {
            anyhow::bail!(
                "judge HTTP {}: {}",
                status,
                text.chars().take(300).collect::<String>()
            );
        }
        let parsed: ChatResponse =
            serde_json::from_str(&text).context("parse judge chat response")?;
        let content = parsed
            .choices
            .first()
            .map(|c| c.message.content.as_str())
            .unwrap_or("");
        parse_grade(content).ok_or_else(|| {
            anyhow::anyhow!(
                "no 0-3 grade in judge output: {:?}",
                content.chars().take(160).collect::<String>()
            )
        })
    }
}

/// The fixed 0–3 relevance rubric. Includes the query verbatim, the passage's
/// file path (a weak provenance hint judges legitimately use) and the passage
/// truncated to keep the prompt bounded. `/no_think` disables qwen3's
/// chain-of-thought for a terse, fast, parseable answer.
pub fn build_rubric_prompt(query: &str, path: &str, passage: &str) -> String {
    let snippet: String = passage.chars().take(1800).collect();
    format!(
        "You are grading how well a code/document passage answers a developer's search query.\n\n\
         Query: {query}\n\n\
         Passage (from `{path}`):\n```\n{snippet}\n```\n\n\
         Relevance scale:\n\
         0 = irrelevant (unrelated to the query)\n\
         1 = marginal (mentions the topic but does not answer it)\n\
         2 = relevant (a meaningful part of the answer)\n\
         3 = highly relevant (directly and substantially answers the query)\n\n\
         Reply with ONLY the single digit 0, 1, 2, or 3. /no_think"
    )
}

/// Extract a 0–3 grade from a judge reply, tolerating qwen3 `<think>` blocks and
/// prefixes/echoes. Strips any reasoning up to the last `</think>`, then takes
/// the **last** 0–3 digit in the remainder — robust to the model echoing the
/// scale ("0 to 3") before stating its actual answer.
pub fn parse_grade(content: &str) -> Option<u8> {
    let tail = match content.rfind("</think>") {
        Some(i) => &content[i + "</think>".len()..],
        None => content,
    };
    tail.chars().rev().find_map(|c| match c {
        '0' => Some(0),
        '1' => Some(1),
        '2' => Some(2),
        '3' => Some(3),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_grade_handles_terse_prefixed_and_thinking() {
        assert_eq!(parse_grade("2"), Some(2));
        assert_eq!(parse_grade("Relevance: 3"), Some(3));
        assert_eq!(parse_grade("<think>could be 1 or 2 …</think>\n2"), Some(2));
        assert_eq!(parse_grade("The answer is 0."), Some(0));
        // Echoes the scale then answers — last digit is the answer.
        assert_eq!(parse_grade("on a scale of 0 to 3 I rate this 1"), Some(1));
        assert_eq!(parse_grade("no grade here"), None);
        assert_eq!(parse_grade(""), None);
    }

    #[test]
    fn rubric_includes_query_path_and_truncates() {
        let long = "x".repeat(5000);
        let p = build_rubric_prompt("how does retry work", "src/retry.rs", &long);
        assert!(p.contains("how does retry work"));
        assert!(p.contains("src/retry.rs"));
        assert!(p.contains("/no_think"));
        // Passage truncated to 1800 chars (+ fixed rubric scaffolding).
        assert!(p.len() < 1800 + 1200, "prompt should bound the passage");
    }
}
