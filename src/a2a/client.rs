//! Outbound A2A client — invoke peer agents from within pgmcp.

#![allow(dead_code)]

use std::time::Duration;

use serde_json::json;
use uuid::Uuid;

use super::types::{Message, Part, Role, Task};

/// A2A client targeting a peer's JSON-RPC endpoint.
#[derive(Clone)]
pub struct A2aClient {
    pub base_url: String,
    pub timeout: Duration,
}

/// Options for an outbound `tasks/send` call.
#[derive(Default, Clone, Copy)]
pub struct SendOptions {
    /// Request N rounds of recursive text refinement (1..=10).
    pub recursion_rounds: Option<u32>,
    /// Correlate this call to a parent orchestration Task.
    pub parent_task_id: Option<Uuid>,
}

impl A2aClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            timeout: Duration::from_secs(60),
        }
    }

    fn http(&self) -> Result<reqwest::Client, String> {
        reqwest::Client::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|e| e.to_string())
    }

    /// `tasks/send`: create a Task on the peer with a text message; returns
    /// the final Task. Synchronous semantics. Calls
    /// [`send_task_with`](Self::send_task_with) with default options.
    pub async fn send_task(&self, text: &str, skill_id: Option<&str>) -> Result<Task, String> {
        self.send_task_with(text, skill_id, SendOptions::default())
            .await
    }

    /// `tasks/send` with explicit options. `recursion_rounds > 1` requests
    /// the peer to iteratively refine its answer over N rounds (paper's
    /// "Recursive-TextMAS" baseline). `parent_task_id` is set when this
    /// call is part of a collaboration-pattern orchestration so the peer
    /// can correlate child tasks back to the parent.
    pub async fn send_task_with(
        &self,
        text: &str,
        skill_id: Option<&str>,
        opts: SendOptions,
    ) -> Result<Task, String> {
        let id = Uuid::new_v4();
        let mut params = serde_json::json!({
            "id": id.to_string(),
            "message": {
                "role": "user",
                "parts": [{"type": "text", "text": text}],
            }
        });
        if let Some(s) = skill_id {
            params["skillId"] = json!(s);
        }
        if let Some(r) = opts.recursion_rounds {
            params["recursionRounds"] = json!(r);
        }
        if let Some(p) = opts.parent_task_id {
            params["parentTaskId"] = json!(p.to_string());
        }
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tasks/send", "params": params,
        });
        let resp = self
            .http()?
            .post(&self.base_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        if let Some(err) = body.get("error") {
            return Err(format!("peer error: {}", err));
        }
        let result = body.get("result").ok_or("no result")?.clone();
        serde_json::from_value(result).map_err(|e| e.to_string())
    }

    /// `tasks/get`: poll Task state on a peer.
    pub async fn get_task(&self, task_id: Uuid) -> Result<Task, String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tasks/get",
            "params": {"id": task_id.to_string()},
        });
        let resp = self
            .http()?
            .post(&self.base_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        if let Some(err) = body.get("error") {
            return Err(format!("peer error: {}", err));
        }
        serde_json::from_value(body.get("result").cloned().unwrap_or_default())
            .map_err(|e| e.to_string())
    }

    /// `tasks/cancel`: cancel a remote Task.
    pub async fn cancel_task(&self, task_id: Uuid) -> Result<Task, String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tasks/cancel",
            "params": {"id": task_id.to_string()},
        });
        let resp = self
            .http()?
            .post(&self.base_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        if let Some(err) = body.get("error") {
            return Err(format!("peer error: {}", err));
        }
        serde_json::from_value(body.get("result").cloned().unwrap_or_default())
            .map_err(|e| e.to_string())
    }
}

/// Build a one-shot user `Message` carrying a single text Part.
pub fn user_text_message(text: impl Into<String>) -> Message {
    Message {
        role: Role::User,
        parts: vec![Part::Text {
            text: text.into(),
            metadata: serde_json::Value::Null,
        }],
    }
}
