//! Memory-server Phase 4: `LlmExtractor` trait.
//!
//! Pluggable backend for LLM-driven salience extraction (Phase 4) and
//! reflection (Phase 5). Matches the design in
//! `docs/memory-server/03-architecture.md` §13.2.
//!
//! Two production backends ship:
//!
//! - **`Qwen3LocalExtractor`** (`qwen3.rs`) — Qwen3-8B-Instruct (or
//!   Qwen3-4B-Instruct) Q4_K_M loaded via candle's `quantized_qwen3`
//!   path. Local; no API calls; uses the user's GPU.
//! - **`CloudExtractor`** (`cloud.rs`) — Anthropic API (Claude Haiku
//!   4.5). Opt-in via `[memory.extractor] backend = "cloud"`. Off by
//!   default.
//!
//! Each backend implements the same trait surface so the salience
//! worker (`src/sessions/extractor_worker.rs`) and the
//! `memory_reflect` MCP tool / cron treat them uniformly.

#![allow(dead_code)] // Phase 4/5 surface; some helpers used only when feature-config selects the cloud or qwen3 backend.

use anyhow::Result;

pub mod cloud;
pub mod extractor_worker;
pub mod latent_pipeline;
pub mod latent_train;
pub mod prompt;
pub mod qwen3;
pub mod qwen3_latent_model;
pub mod recursive_link;
pub mod reflect;
pub mod remote;

/// Reference to an existing entity passed to the extractor as grounding
/// context. Keeping payload small here keeps the prompt cheap: we send
/// (name, type, top-K observations) per existing entity, not the full
/// observation history.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EntityRef {
    pub id: i64,
    pub name: String,
    pub entity_type: String,
    pub key_observations: Vec<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ScopeRef {
    pub user_id: Option<String>,
    pub agent_id: Option<String>,
    pub session_id: Option<uuid::Uuid>,
    pub project_id: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct ExtractionRequest<'a> {
    pub text: &'a str,
    pub existing_entities: &'a [EntityRef],
    pub scope: &'a ScopeRef,
    /// Cap on how many distinct facts to emit per extraction call.
    /// Defaults to 25 in the worker; bounded by the prompt template.
    pub max_extractions: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NewEntity {
    pub name: String,
    pub entity_type: String,
    #[serde(default)]
    pub initial_observations: Vec<String>,
    /// LLM-judged importance in [0, 1]. Drives the auto-promote threshold
    /// in the worker and the eviction policy in Phase 8.
    #[serde(default = "default_importance")]
    pub importance: f32,
}

fn default_importance() -> f32 {
    0.5
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NewRelation {
    pub from_name: String,
    pub to_name: String,
    pub relation_type: String,
    #[serde(default = "default_importance")]
    pub importance: f32,
}

/// Variant of "what kind of contradiction did the LLM see". Observations
/// and relations have separate invalidation paths so we keep the kind
/// explicit rather than overloading a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContradictionKind {
    Observation,
    Relation,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContradictionSignal {
    /// id of the existing observation or relation that is now invalidated.
    pub conflicting_with: i64,
    pub kind: ContradictionKind,
    pub reason: String,
}

/// Full structured payload returned by the LLM. The worker validates
/// this against the `schemars` schema before any DB write — if parsing
/// fails the entire batch is dropped (no half-extracted facts).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ExtractionResult {
    #[serde(default)]
    pub entities: Vec<NewEntity>,
    #[serde(default)]
    pub relations: Vec<NewRelation>,
    #[serde(default)]
    pub contradictions: Vec<ContradictionSignal>,
}

/// Extractor abstraction. Both `extract` and `reflect` are synchronous
/// (the candle inference path is naturally sync); the worker wraps each
/// call in `tokio::task::spawn_blocking` so the runtime stays
/// responsive.
pub trait LlmExtractor: Send + Sync {
    /// Human-readable backend name for logging.
    fn name(&self) -> &'static str;
    /// Versioned model signature (e.g. `"qwen3-8b-instruct-q4km-v1"`,
    /// `"anthropic-haiku-4.5-v1"`). Stamped on emitted observations
    /// via `memory_observations.embedding_signature`-style provenance
    /// for downstream auditability.
    fn model_signature(&self) -> &'static str;
    /// One-shot extraction call. Implementations format the prompt,
    /// invoke the model, validate the response JSON, and return the
    /// parsed result.
    fn extract(&self, request: ExtractionRequest<'_>) -> Result<ExtractionResult>;
    /// Reflection call: given a set of recent observation contents,
    /// emit higher-order observations / patterns / preferences.
    /// `derived_from` linkage is the caller's responsibility (the
    /// worker has the observation ids).
    fn reflect(&self, observations: &[String]) -> Result<Vec<NewEntity>>;
}

/// Closed-set backend selector. Add variants here when adding a new
/// backend; the `make_extractor` factory dispatches on this.
#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LlmBackendChoice {
    Qwen38b,
    Qwen34b,
    Cloud(CloudProvider),
    /// Remote OpenAI-compatible endpoint (Crucible E1). Lets the daemon offload
    /// extraction/reflection to a network-reachable server (e.g. sparky's ollama
    /// or DeepSeek-V4), freeing the contended local GPU. Endpoint/model come from
    /// `PGMCP_LLM_BASE_URL` / `PGMCP_LLM_MODEL` (see `remote::RemoteOpenAiExtractor`).
    /// Unit variant so this enum stays `Copy`.
    Remote,
    /// Disable LLM extraction entirely. The salience worker falls back
    /// to the existing regex pipeline (Stage A); Stage B is a no-op.
    Disabled,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CloudProvider {
    Anthropic,
}

/// Parse `[memory.extractor] backend` config strings into a backend choice.
pub fn parse_backend_choice(s: &str) -> Result<LlmBackendChoice> {
    match s {
        "qwen3-8b" | "qwen3-8B" => Ok(LlmBackendChoice::Qwen38b),
        "qwen3-4b" | "qwen3-4B" => Ok(LlmBackendChoice::Qwen34b),
        "cloud" | "anthropic" | "cloud-anthropic" => {
            Ok(LlmBackendChoice::Cloud(CloudProvider::Anthropic))
        }
        "remote-openai" | "remote" | "openai-remote" => Ok(LlmBackendChoice::Remote),
        "disabled" | "off" | "none" => Ok(LlmBackendChoice::Disabled),
        other => Err(anyhow::anyhow!(
            "unknown LLM extractor backend '{}'; choices: qwen3-8b, qwen3-4b, cloud, remote-openai, disabled",
            other
        )),
    }
}

/// Factory mirroring the FcmBackend pattern in `src/fcm/mod.rs:178`.
/// Constructs the backend per `choice`; `Disabled` returns
/// `Ok(None)` so the caller can no-op rather than fail.
pub fn make_extractor(choice: LlmBackendChoice) -> Result<Option<Box<dyn LlmExtractor>>> {
    match choice {
        LlmBackendChoice::Disabled => Ok(None),
        LlmBackendChoice::Cloud(CloudProvider::Anthropic) => {
            Ok(Some(Box::new(cloud::AnthropicExtractor::new()?)))
        }
        LlmBackendChoice::Qwen38b => Ok(Some(Box::new(qwen3::Qwen3LocalExtractor::new(
            qwen3::Qwen3Variant::Eight,
        )?))),
        LlmBackendChoice::Qwen34b => Ok(Some(Box::new(qwen3::Qwen3LocalExtractor::new(
            qwen3::Qwen3Variant::Four,
        )?))),
        LlmBackendChoice::Remote => Ok(Some(Box::new(remote::RemoteOpenAiExtractor::from_env()?))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_backend_choice_round_trip() {
        assert!(matches!(
            parse_backend_choice("qwen3-8b").unwrap(),
            LlmBackendChoice::Qwen38b
        ));
        assert!(matches!(
            parse_backend_choice("qwen3-4b").unwrap(),
            LlmBackendChoice::Qwen34b
        ));
        assert!(matches!(
            parse_backend_choice("cloud").unwrap(),
            LlmBackendChoice::Cloud(CloudProvider::Anthropic)
        ));
        assert!(matches!(
            parse_backend_choice("disabled").unwrap(),
            LlmBackendChoice::Disabled
        ));
        assert!(matches!(
            parse_backend_choice("remote-openai").unwrap(),
            LlmBackendChoice::Remote
        ));
        assert!(matches!(
            parse_backend_choice("remote").unwrap(),
            LlmBackendChoice::Remote
        ));
        assert!(parse_backend_choice("bogus").is_err());
    }

    #[test]
    fn extraction_result_round_trips_json() {
        let r = ExtractionResult {
            entities: vec![NewEntity {
                name: "rust".into(),
                entity_type: "language".into(),
                initial_observations: vec!["ownership".into()],
                importance: 0.7,
            }],
            relations: vec![NewRelation {
                from_name: "rust".into(),
                to_name: "memory_safety".into(),
                relation_type: "guarantees".into(),
                importance: 0.6,
            }],
            contradictions: vec![],
        };
        let j = serde_json::to_string(&r).expect("ser");
        let back: ExtractionResult = serde_json::from_str(&j).expect("de");
        assert_eq!(back.entities.len(), 1);
        assert_eq!(back.relations.len(), 1);
    }
}
