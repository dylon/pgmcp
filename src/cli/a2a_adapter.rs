//! `pgmcp a2a-adapter` — expose a CLI agent (Claude Code / Codex) as a
//! live A2A peer (Part A phase A5a).
//!
//! Binds a minimal A2A JSON-RPC server that translates an inbound
//! `tasks/send` into a `claude -p {{message}}` / `codex {{message}}`
//! subprocess invocation (via the existing `GenericSubprocessAdapter`) and
//! returns the CLI's stdout as a Completed Task artifact. This is what
//! turns a non-A2A-native CLI into a recursive sub-callable peer for the
//! RLM loop (Part B) and the collaboration patterns.
//!
//! Deliberately standalone: it reuses `a2a::types` + the adapter structs
//! but needs no `ApiState`, embedder, or database — it is a stateless leaf
//! LLM peer (string in → string out), matching the RLM paper's sub-LM.

use std::sync::Arc;

use anyhow::{Result, bail};
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::{get, post};
use chrono::Utc;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::a2a::adapters::GenericSubprocessAdapter;
use crate::a2a::types::{
    Artifact, JsonRpcRequest, JsonRpcResponse, Message, Part, Role, Task, TaskState, TaskStatus,
};

#[derive(Clone)]
struct AdapterState {
    adapter: Arc<GenericSubprocessAdapter>,
    name: Arc<String>,
}

/// Entry point for the `a2a-adapter` subcommand (initializes CLI logging,
/// then serves).
pub async fn run(
    kind: String,
    port: u16,
    name: Option<String>,
    register_with: Option<String>,
    pi_provider: Option<String>,
    pi_model: Option<String>,
) -> Result<()> {
    crate::logging::init_cli_with_config(None);
    serve_adapter(kind, port, name, register_with, pi_provider, pi_model).await
}

/// Embedded entry point for daemon autostart (`[a2a] autostart_adapters`).
/// Identical to `run` but skips logging init — the daemon already initialized
/// tracing. Meant to be `tokio::spawn`ed; serves until the process exits.
pub async fn run_embedded(
    kind: String,
    port: u16,
    name: Option<String>,
    register_with: Option<String>,
    pi_provider: Option<String>,
    pi_model: Option<String>,
) -> Result<()> {
    serve_adapter(kind, port, name, register_with, pi_provider, pi_model).await
}

async fn serve_adapter(
    kind: String,
    port: u16,
    name: Option<String>,
    register_with: Option<String>,
    pi_provider: Option<String>,
    pi_model: Option<String>,
) -> Result<()> {
    let (inner, default_name, description) = match kind.as_str() {
        "claude" => (
            crate::a2a::adapters::ClaudeCodeAdapter::new().inner,
            "claude-code",
            "Claude Code CLI exposed as an A2A peer (claude -p)",
        ),
        "codex" => (
            crate::a2a::adapters::CodexCliAdapter::new().inner,
            "codex-cli",
            "OpenAI Codex CLI exposed as an A2A peer (codex)",
        ),
        "pi" => (
            crate::a2a::adapters::PiAdapter::new(pi_provider, pi_model).inner,
            "pi-agent",
            "pi coding agent exposed as an A2A peer (pi -p, MCP-free leaf)",
        ),
        other => {
            bail!("unknown adapter kind '{other}'; expected 'claude', 'codex', or 'pi'")
        }
    };
    let agent_name = name.unwrap_or_else(|| default_name.to_string());

    let state = AdapterState {
        adapter: Arc::new(inner),
        name: Arc::new(agent_name.clone()),
    };

    let app = Router::new()
        .route("/.well-known/agent.json", get(get_agent_card))
        .route("/a2a/jsonrpc", post(handle_jsonrpc))
        .with_state(state);

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow::anyhow!("bind {addr}: {e}"))?;
    tracing::info!(agent = %agent_name, %addr, kind = %kind, "a2a-adapter listening");

    // Self-register with a pgmcp daemon so its agent registry can route to us.
    if let Some(daemon) = register_with.as_deref() {
        self_register(daemon, &agent_name, port, description).await;
    }

    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("a2a-adapter serve: {e}"))?;
    Ok(())
}

/// Minimal AgentCard for discovery. Hand-built JSON (peers route via the
/// daemon's `a2a_agents` registry, not by parsing this card), so we avoid
/// coupling to the full `AgentCard` struct.
async fn get_agent_card(State(state): State<AdapterState>) -> Json<Value> {
    Json(json!({
        "name": &*state.name,
        "version": env!("CARGO_PKG_VERSION"),
        "description": "CLI agent exposed as an A2A peer by pgmcp a2a-adapter.",
        "capabilities": { "streaming": false, "pushNotifications": false, "stateTransitionHistory": false },
        "authentication": { "schemes": ["none"] },
        "defaultInputModes": ["text"],
        "defaultOutputModes": ["text"],
        "skills": [{
            "id": "reasoning",
            "name": "General reasoning",
            "description": "Forward the task prompt to the wrapped CLI and return its answer.",
            "tags": ["reasoning", "planning"],
            "specialty": ["reasoning", "planning"],
            "recommendedRole": "Solver"
        }]
    }))
}

async fn handle_jsonrpc(
    State(state): State<AdapterState>,
    Json(req): Json<JsonRpcRequest>,
) -> Json<JsonRpcResponse> {
    if req.jsonrpc != "2.0" {
        return Json(JsonRpcResponse::error(
            req.id,
            -32600,
            "jsonrpc must be \"2.0\"",
        ));
    }
    match req.method.as_str() {
        // Both the unary and the subscribe variant run synchronously here —
        // the adapter is a leaf peer, so there is nothing to stream.
        "tasks/send" | "tasks/sendSubscribe" => {
            let id = req.id.clone();
            let text = extract_message_text(&req.params);
            if text.trim().is_empty() {
                return Json(JsonRpcResponse::error(id, -32602, "message text required"));
            }
            let task_id = req
                .params
                .get("id")
                .and_then(|x| x.as_str())
                .and_then(|s| Uuid::parse_str(s).ok())
                .unwrap_or_else(Uuid::new_v4);
            let parent = req
                .params
                .get("parentTaskId")
                .and_then(|x| x.as_str())
                .and_then(|s| Uuid::parse_str(s).ok());
            match state.adapter.execute(&text).await {
                Ok(output) => {
                    let task = build_task(task_id, parent, &text, &state.name, output);
                    Json(JsonRpcResponse::success(
                        id,
                        serde_json::to_value(task).unwrap_or_else(|_| json!({})),
                    ))
                }
                Err(e) => Json(JsonRpcResponse::error(
                    id,
                    -32603,
                    format!("adapter subprocess failed: {e}"),
                )),
            }
        }
        other => Json(JsonRpcResponse::error(
            req.id,
            -32601,
            format!("method not supported by adapter: {other}"),
        )),
    }
}

/// Concatenate the text of all text Parts in `params.message.parts`.
fn extract_message_text(params: &Value) -> String {
    let mut out = String::new();
    if let Some(parts) = params
        .get("message")
        .and_then(|m| m.get("parts"))
        .and_then(|p| p.as_array())
    {
        for part in parts {
            let is_text = part.get("type").and_then(|t| t.as_str()) == Some("text");
            if is_text && let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                out.push_str(t);
                out.push('\n');
            }
        }
    }
    out
}

/// Build a Completed Task carrying the subprocess output as one text artifact.
fn build_task(
    id: Uuid,
    parent: Option<Uuid>,
    user_text: &str,
    agent_name: &str,
    output: String,
) -> Task {
    Task {
        id,
        session_id: None,
        status: TaskStatus {
            state: TaskState::Completed,
            message: None,
            timestamp: Utc::now(),
        },
        history: Some(vec![Message {
            role: Role::User,
            parts: vec![Part::Text {
                text: user_text.to_string(),
                metadata: Value::Null,
            }],
            metadata: Value::Null,
        }]),
        artifacts: vec![Artifact {
            name: Some(agent_name.to_string()),
            parts: vec![Part::Text {
                text: output,
                metadata: Value::Null,
            }],
            index: 0,
            append: false,
            last_chunk: true,
            metadata: Value::Null,
        }],
        metadata: Value::Null,
        recursion_rounds: 1,
        current_round: 1,
        parent_task_id: parent,
    }
}

/// Best-effort POST to a pgmcp daemon's `/a2a/agents` registry. Failures
/// are logged and non-fatal (the operator can register manually via the
/// `a2a_register_agent` MCP tool).
async fn self_register(daemon_url: &str, name: &str, port: u16, description: &str) {
    let url = format!("http://127.0.0.1:{port}/a2a/jsonrpc");
    let endpoint = format!("{}/a2a/agents", daemon_url.trim_end_matches('/'));
    let payload = json!({
        "name": name,
        "url": url,
        "version": env!("CARGO_PKG_VERSION"),
        "description": description,
        "specialty": ["reasoning", "planning"],
        "recommendedRole": "Solver",
    });
    let client = reqwest::Client::new();
    // Bounded retry: when autostarted in-process, the daemon's HTTP server may
    // not be accepting yet at the instant we register. Best-effort and never
    // fatal — the operator can always register manually via `a2a_register_agent`.
    for attempt in 1..=5u32 {
        match client.post(&endpoint).json(&payload).send().await {
            Ok(r) if r.status().is_success() => {
                tracing::info!(agent = name, endpoint = %endpoint, "self-registered with daemon");
                return;
            }
            Ok(r) => tracing::warn!(
                status = %r.status(), attempt, endpoint = %endpoint, "self-register non-success"
            ),
            Err(e) => tracing::warn!(
                error = %e, attempt, endpoint = %endpoint, "self-register failed"
            ),
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    tracing::warn!(
        agent = name, endpoint = %endpoint,
        "self-register gave up after retries; register manually via a2a_register_agent"
    );
}
