//! JSON-RPC method dispatch for the A2A server.

#![allow(dead_code)]

use chrono::Utc;
use serde_json::json;
use uuid::Uuid;

use crate::api::ApiState;

use super::dispatcher;
use super::types::{
    JsonRpcRequest, JsonRpcResponse, Message, Part, Role, Task, TaskState, TaskStatus,
};

const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;

pub async fn dispatch(state: &ApiState, req: JsonRpcRequest) -> JsonRpcResponse {
    if req.jsonrpc != "2.0" {
        return JsonRpcResponse::error(req.id, INVALID_REQUEST, "jsonrpc must be \"2.0\"");
    }
    match req.method.as_str() {
        "tasks/send" => tasks_send(state, req).await,
        "tasks/sendSubscribe" => tasks_send_subscribe(state, req).await,
        "tasks/get" => tasks_get(state, req).await,
        "tasks/cancel" => tasks_cancel(state, req).await,
        "tasks/pushNotification/set" => tasks_push_set(state, req).await,
        "tasks/pushNotification/get" => tasks_push_get(state, req).await,
        "tasks/resubscribe" => tasks_resubscribe(state, req).await,
        other => JsonRpcResponse::error(
            req.id,
            METHOD_NOT_FOUND,
            format!("Method not found: {}", other),
        ),
    }
}

async fn tasks_send(state: &ApiState, req: JsonRpcRequest) -> JsonRpcResponse {
    let id = req.id.clone();
    let params = match parse_send_params(&req.params) {
        Ok(p) => p,
        Err(e) => return JsonRpcResponse::error(id, INVALID_PARAMS, e),
    };
    match dispatcher::create_and_start(state, params).await {
        Ok(task) => JsonRpcResponse::success(id, serde_json::to_value(task).unwrap_or(json!({}))),
        Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e),
    }
}

async fn tasks_send_subscribe(state: &ApiState, req: JsonRpcRequest) -> JsonRpcResponse {
    // For SSE streaming, the client should follow up with GET /a2a/sse/{task_id}.
    // This method creates the Task and returns its id + the SSE URL.
    let id = req.id.clone();
    let params = match parse_send_params(&req.params) {
        Ok(p) => p,
        Err(e) => return JsonRpcResponse::error(id, INVALID_PARAMS, e),
    };
    match dispatcher::create_and_start(state, params).await {
        Ok(task) => {
            let sse_url = format!("/a2a/sse/{}", task.id);
            JsonRpcResponse::success(id, json!({ "task": task, "sseUrl": sse_url }))
        }
        Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e),
    }
}

async fn tasks_get(state: &ApiState, req: JsonRpcRequest) -> JsonRpcResponse {
    let id = req.id.clone();
    let task_id = match parse_task_id(&req.params) {
        Ok(t) => t,
        Err(e) => return JsonRpcResponse::error(id, INVALID_PARAMS, e),
    };
    match dispatcher::get_task(state, task_id).await {
        Ok(Some(t)) => JsonRpcResponse::success(id, serde_json::to_value(t).unwrap_or(json!({}))),
        Ok(None) => JsonRpcResponse::error(id, INVALID_PARAMS, "Task not found"),
        Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e),
    }
}

async fn tasks_cancel(state: &ApiState, req: JsonRpcRequest) -> JsonRpcResponse {
    let id = req.id.clone();
    let task_id = match parse_task_id(&req.params) {
        Ok(t) => t,
        Err(e) => return JsonRpcResponse::error(id, INVALID_PARAMS, e),
    };
    match dispatcher::cancel_task(state, task_id).await {
        Ok(t) => JsonRpcResponse::success(id, serde_json::to_value(t).unwrap_or(json!({}))),
        Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e),
    }
}

async fn tasks_push_set(state: &ApiState, req: JsonRpcRequest) -> JsonRpcResponse {
    let id = req.id.clone();
    let task_id = match parse_task_id(&req.params) {
        Ok(t) => t,
        Err(e) => return JsonRpcResponse::error(id, INVALID_PARAMS, e),
    };
    let url = req
        .params
        .get("pushNotificationConfig")
        .and_then(|v| v.get("url"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let url = match url {
        Some(u) => u,
        None => {
            return JsonRpcResponse::error(
                id,
                INVALID_PARAMS,
                "pushNotificationConfig.url required",
            );
        }
    };
    match dispatcher::set_push_notification(state, task_id, &url).await {
        Ok(_) => JsonRpcResponse::success(id, json!({"taskId": task_id, "pushUrl": url})),
        Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e),
    }
}

async fn tasks_push_get(state: &ApiState, req: JsonRpcRequest) -> JsonRpcResponse {
    let id = req.id.clone();
    let task_id = match parse_task_id(&req.params) {
        Ok(t) => t,
        Err(e) => return JsonRpcResponse::error(id, INVALID_PARAMS, e),
    };
    match dispatcher::get_push_notification(state, task_id).await {
        Ok(url) => JsonRpcResponse::success(id, json!({"taskId": task_id, "pushUrl": url})),
        Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e),
    }
}

async fn tasks_resubscribe(state: &ApiState, req: JsonRpcRequest) -> JsonRpcResponse {
    // Resubscribe to a streaming Task — return the task + SSE URL.
    let id = req.id.clone();
    let task_id = match parse_task_id(&req.params) {
        Ok(t) => t,
        Err(e) => return JsonRpcResponse::error(id, INVALID_PARAMS, e),
    };
    match dispatcher::get_task(state, task_id).await {
        Ok(Some(t)) => JsonRpcResponse::success(
            id,
            json!({ "task": t, "sseUrl": format!("/a2a/sse/{}", task_id) }),
        ),
        Ok(None) => JsonRpcResponse::error(id, INVALID_PARAMS, "Task not found"),
        Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e),
    }
}

/// `tasks/send` params struct (subset of A2A spec).
#[derive(Debug, Clone)]
pub struct SendParams {
    pub id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub message: Message,
    pub skill_id: Option<String>,
    pub push_notification_url: Option<String>,
    /// Optional RecursiveMAS-style refinement rounds. When > 1 the dispatcher
    /// invokes the skill N times, threading each round's output as
    /// conditioning context into the next. Clamped to 1..=10 in the
    /// dispatcher to bound runaway cost.
    pub recursion_rounds: Option<u32>,
    /// Optional parent Task linking this task to a collaboration-pattern
    /// orchestration parent (Sequential / Mixture / Distillation /
    /// Deliberation). When `None` this is a standalone task.
    pub parent_task_id: Option<Uuid>,
}

fn parse_send_params(v: &serde_json::Value) -> Result<SendParams, String> {
    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .and_then(|s| Uuid::parse_str(s).ok());
    let session_id = v
        .get("sessionId")
        .and_then(|x| x.as_str())
        .and_then(|s| Uuid::parse_str(s).ok());
    let message_val = v.get("message").ok_or("`message` required")?;
    let message: Message = serde_json::from_value(message_val.clone())
        .map_err(|e| format!("invalid message: {}", e))?;
    let skill_id = v
        .get("skillId")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let push_notification_url = v
        .get("pushNotificationConfig")
        .and_then(|c| c.get("url"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string());
    let recursion_rounds = v
        .get("recursionRounds")
        .and_then(|x| x.as_u64())
        .map(|n| n as u32);
    let parent_task_id = v
        .get("parentTaskId")
        .and_then(|x| x.as_str())
        .and_then(|s| Uuid::parse_str(s).ok());
    Ok(SendParams {
        id,
        session_id,
        message,
        skill_id,
        push_notification_url,
        recursion_rounds,
        parent_task_id,
    })
}

fn parse_task_id(v: &serde_json::Value) -> Result<Uuid, String> {
    let s = v
        .get("id")
        .and_then(|x| x.as_str())
        .ok_or("`id` required")?;
    Uuid::parse_str(s).map_err(|e| format!("bad uuid: {}", e))
}

/// Helper: minimal Task seed used when creating.
pub fn initial_task(id: Uuid, session_id: Option<Uuid>, msg: Message) -> Task {
    Task {
        id,
        session_id,
        status: TaskStatus {
            state: TaskState::Submitted,
            message: None,
            timestamp: Utc::now(),
        },
        history: Some(vec![msg]),
        artifacts: Vec::new(),
        metadata: serde_json::Value::Null,
        recursion_rounds: 1,
        current_round: 0,
        parent_task_id: None,
    }
}

/// Helper: build a text-only agent message.
pub fn text_message(text: &str) -> Message {
    Message {
        role: Role::Agent,
        parts: vec![Part::Text {
            text: text.to_string(),
            metadata: serde_json::Value::Null,
        }],
        metadata: serde_json::Value::Null,
    }
}

/// Helper: build a text-only user message. Used by the recursion loop to
/// construct round-N prompts that thread previous output as conditioning.
pub fn user_text_message(text: &str) -> Message {
    Message {
        role: Role::User,
        parts: vec![Part::Text {
            text: text.to_string(),
            metadata: serde_json::Value::Null,
        }],
        metadata: serde_json::Value::Null,
    }
}
