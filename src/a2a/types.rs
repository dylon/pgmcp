//! A2A wire-level types — minimal but spec-faithful subset.
//!
//! Reference: https://google.github.io/A2A/

#![allow(dead_code)]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCard {
    pub name: String,
    pub version: String,
    pub description: String,
    pub url: String,
    pub provider: AgentProvider,
    pub capabilities: AgentCapabilities,
    pub authentication: AgentAuthentication,
    #[serde(rename = "defaultInputModes")]
    pub default_input_modes: Vec<String>,
    #[serde(rename = "defaultOutputModes")]
    pub default_output_modes: Vec<String>,
    pub skills: Vec<AgentSkill>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProvider {
    pub organization: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCapabilities {
    pub streaming: bool,
    #[serde(rename = "pushNotifications")]
    pub push_notifications: bool,
    #[serde(rename = "stateTransitionHistory")]
    pub state_transition_history: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentAuthentication {
    pub schemes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
    /// Machine-readable specialty tags for orchestration routing.
    /// e.g. `["search", "retrieval"]`, `["graph", "architecture"]`.
    /// Inspired by Yang et al. 2026 RecursiveMAS Table 1's role-specific
    /// model selection (Math Specialist, Code Specialist, etc.).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub specialty: Vec<String>,
    /// Suggested collaboration role for this skill
    /// (e.g. `"Search Specialist"`, `"Critic"`, `"Summarizer"`,
    /// `"Reflector"`, `"Tool-Caller"`). Used by the orchestration patterns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_role: Option<String>,
}

/// A2A Task lifecycle state. See A2A spec for state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskState {
    Submitted,
    Working,
    InputRequired,
    Completed,
    Canceled,
    Failed,
}

impl TaskState {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            TaskState::Submitted => "submitted",
            TaskState::Working => "working",
            TaskState::InputRequired => "input-required",
            TaskState::Completed => "completed",
            TaskState::Canceled => "canceled",
            TaskState::Failed => "failed",
        }
    }
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "working" => TaskState::Working,
            "input-required" => TaskState::InputRequired,
            "completed" => TaskState::Completed,
            "canceled" => TaskState::Canceled,
            "failed" => TaskState::Failed,
            _ => TaskState::Submitted,
        }
    }
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Completed | TaskState::Canceled | TaskState::Failed
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Uuid,
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    pub status: TaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<Message>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// Number of recursion rounds requested for this Task. 1 = single pass.
    /// Inspired by Yang et al. 2026 "Recursive Multi-Agent Systems" Section 5.
    #[serde(rename = "recursionRounds", default = "default_recursion_rounds")]
    pub recursion_rounds: u32,
    /// Latest completed round index (0..recursion_rounds). Updates as the
    /// dispatcher progresses through the inner refinement loop.
    #[serde(rename = "currentRound", default)]
    pub current_round: u32,
    /// Parent Task ID when this Task was spawned as part of a collaboration
    /// pattern (Sequential / Mixture / Distillation / Deliberation).
    #[serde(rename = "parentTaskId", skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<Uuid>,
}

fn default_recursion_rounds() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskStatus {
    pub state: TaskState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    User,
    Agent,
}

impl Role {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Agent => "agent",
        }
    }
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "agent" => Role::Agent,
            _ => Role::User,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Part {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        metadata: serde_json::Value,
    },
    File {
        file: FilePart,
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        metadata: serde_json::Value,
    },
    Data {
        data: serde_json::Value,
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        metadata: serde_json::Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePart {
    pub name: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<String>, // base64
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub parts: Vec<Part>,
    #[serde(default)]
    pub index: i32,
    #[serde(default)]
    pub append: bool,
    #[serde(rename = "lastChunk", default)]
    pub last_chunk: bool,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
}

/// SSE event emitted for `sendSubscribe` subscribers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Status {
        task_id: Uuid,
        status: TaskStatus,
        #[serde(default)]
        r#final: bool,
    },
    Message {
        task_id: Uuid,
        message: Message,
    },
    Artifact {
        task_id: Uuid,
        artifact: Artifact,
    },
    Final {
        task_id: Uuid,
        task: Task,
    },
}

/// JSON-RPC envelope (request).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// JSON-RPC envelope (response).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
}

impl JsonRpcResponse {
    pub fn success(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }
    pub fn error(id: Option<serde_json::Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: serde_json::Value::Null,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_state_round_trips() {
        for s in [
            TaskState::Submitted,
            TaskState::Working,
            TaskState::InputRequired,
            TaskState::Completed,
            TaskState::Canceled,
            TaskState::Failed,
        ] {
            assert_eq!(TaskState::from_db_str(s.as_db_str()), s);
        }
    }

    #[test]
    fn task_state_terminal_check() {
        assert!(TaskState::Completed.is_terminal());
        assert!(TaskState::Canceled.is_terminal());
        assert!(TaskState::Failed.is_terminal());
        assert!(!TaskState::Submitted.is_terminal());
        assert!(!TaskState::Working.is_terminal());
        assert!(!TaskState::InputRequired.is_terminal());
    }

    #[test]
    fn agent_card_serializes_camel_case() {
        let card = AgentCard {
            name: "test".into(),
            version: "0.1.0".into(),
            description: "test".into(),
            url: "http://localhost:3100".into(),
            provider: AgentProvider {
                organization: "test-org".into(),
            },
            capabilities: AgentCapabilities {
                streaming: true,
                push_notifications: true,
                state_transition_history: true,
            },
            authentication: AgentAuthentication {
                schemes: vec!["none".into()],
            },
            default_input_modes: vec!["text".into()],
            default_output_modes: vec!["text".into()],
            skills: vec![AgentSkill {
                id: "demo".into(),
                name: "Demo".into(),
                description: "test skill".into(),
                tags: vec!["demo".into()],
                examples: vec![],
                specialty: vec!["search".into(), "retrieval".into()],
                recommended_role: Some("Search Specialist".into()),
            }],
        };
        let json = serde_json::to_string(&card).expect("serialize");
        assert!(json.contains("\"pushNotifications\":true"));
        assert!(json.contains("\"stateTransitionHistory\":true"));
        assert!(json.contains("\"defaultInputModes\""));
        // New AgentSkill fields must serialize as camelCase.
        assert!(
            json.contains("\"specialty\":[\"search\",\"retrieval\"]"),
            "specialty array missing from {}",
            json
        );
        assert!(
            json.contains("\"recommendedRole\":\"Search Specialist\""),
            "recommendedRole missing from {}",
            json
        );
    }

    #[test]
    fn part_text_roundtrip() {
        let p = Part::Text {
            text: "hello".into(),
            metadata: serde_json::Value::Null,
        };
        let json = serde_json::to_string(&p).expect("ser");
        let back: Part = serde_json::from_str(&json).expect("de");
        match back {
            Part::Text { text, .. } => assert_eq!(text, "hello"),
            _ => panic!("expected text part"),
        }
    }

    #[test]
    fn jsonrpc_success_shape() {
        let r =
            JsonRpcResponse::success(Some(serde_json::json!(1)), serde_json::json!({"ok":true}));
        let json = serde_json::to_string(&r).expect("ser");
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"result\":{\"ok\":true}"));
        assert!(!json.contains("\"error\""));
    }
}
