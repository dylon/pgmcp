//! A2A Task dispatcher.
//!
//! Translates incoming A2A Tasks into MCP tool invocations and persists
//! the lifecycle (status, messages, artifacts, events) in the `a2a_*`
//! schema. Sync invocations happen inline (the `tasks/send` call returns
//! the final Task). Streaming via `/a2a/sse/{task_id}` reads from the
//! `a2a_events` table via Postgres LISTEN/NOTIFY (see `sse.rs`).

#![allow(dead_code)]

use std::sync::atomic::Ordering;

use chrono::Utc;
use serde_json::json;
use uuid::Uuid;

use crate::api::ApiState;

use super::handlers::{SendParams, initial_task, text_message};
use super::sse;
use super::types::{Artifact, Message, Part, Role, Task, TaskState, TaskStatus};

/// Safety cap for recursion. The paper shows diminishing returns past r=3-4;
/// 10 is a hard upper bound to bound runaway cost for closed-peer agents.
const MAX_RECURSION_ROUNDS: u32 = 10;

/// Create a Task row and dispatch it. Returns the final Task once the
/// underlying MCP tool returns (sync semantics — A2A `tasks/send`).
///
/// When `params.recursion_rounds > 1`, the skill is invoked N times,
/// threading each round's output as conditioning context into the next.
/// This implements the paper's "Recursive-TextMAS" baseline (Yang et al.
/// 2026 Section 5).
pub async fn create_and_start(state: &ApiState, params: SendParams) -> Result<Task, String> {
    let task_id = params.id.unwrap_or_else(Uuid::new_v4);
    let pool = state.db.pool().ok_or("no pool")?;
    let rounds = params
        .recursion_rounds
        .unwrap_or(1)
        .clamp(1, MAX_RECURSION_ROUNDS);

    // Insert Task row with recursion + parent metadata.
    sqlx::query(
        "INSERT INTO a2a_tasks
            (id, session_id, skill_id, status, push_notification_url, metadata,
             recursion_rounds, current_round, parent_task_id)
         VALUES ($1, $2, $3, 'submitted', $4, '{}'::jsonb, $5, 0, $6)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(task_id)
    .bind(params.session_id)
    .bind(&params.skill_id)
    .bind(&params.push_notification_url)
    .bind(rounds as i32)
    .bind(params.parent_task_id)
    .execute(pool)
    .await
    .map_err(|e| format!("insert task: {}", e))?;

    // Persist the inbound user message.
    let msg_json = serde_json::to_value(&params.message.parts).map_err(|e| e.to_string())?;
    sqlx::query(
        "INSERT INTO a2a_messages (task_id, role, parts, sequence)
         VALUES ($1, 'user', $2, 0)",
    )
    .bind(task_id)
    .bind(&msg_json)
    .execute(pool)
    .await
    .map_err(|e| format!("insert msg: {}", e))?;

    // Mark working.
    set_state(state, task_id, TaskState::Working, None).await?;
    state
        .stats
        .a2a_tasks_created
        .fetch_add(1, Ordering::Relaxed);

    let original_text = extract_text(&params.message);
    let skill = params
        .skill_id
        .clone()
        .unwrap_or_else(|| "code-analysis".into());

    // Recursive refinement loop.
    let mut prev_output: Option<String> = None;
    let mut last_parts: Vec<Part> = Vec::new();
    let mut last_err: Option<String> = None;
    for round in 0..rounds {
        let prompt_text = match &prev_output {
            None => original_text.clone(),
            Some(prev) => format!(
                "Original query:\n{original}\n\n\
                 Round {round} previous output:\n{prev}\n\n\
                 Refine the previous output. Improve correctness, \
                 completeness, and clarity.",
                original = original_text,
                round = round,
                prev = prev,
            ),
        };
        let round_msg = super::handlers::user_text_message(&prompt_text);
        match invoke_skill(state, task_id, &skill, &prompt_text, &round_msg).await {
            Ok(parts) => {
                insert_artifact_round(state, task_id, round as i32, parts.clone()).await?;
                set_current_round(state, task_id, (round + 1) as i32).await?;
                prev_output = Some(parts_to_text(&parts));
                last_parts = parts;
                state
                    .stats
                    .a2a_recursive_rounds_executed
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                last_err = Some(e);
                break;
            }
        }
    }

    let final_task = match last_err {
        None => {
            set_state(state, task_id, TaskState::Completed, None).await?;
            state
                .stats
                .a2a_tasks_completed
                .fetch_add(1, Ordering::Relaxed);
            let mut t = initial_task(task_id, params.session_id, params.message);
            t.status = TaskStatus {
                state: TaskState::Completed,
                message: Some(text_message("Task completed")),
                timestamp: Utc::now(),
            };
            t.recursion_rounds = rounds;
            t.current_round = rounds;
            t.parent_task_id = params.parent_task_id;
            t.artifacts.push(Artifact {
                name: Some(skill),
                parts: last_parts,
                index: 0,
                append: false,
                last_chunk: true,
                metadata: serde_json::Value::Null,
            });
            // Fire push notification if registered.
            if let Some(url) = &params.push_notification_url {
                let _ = fire_push_notification(url, &t).await;
            }
            t
        }
        Some(err) => {
            set_state(state, task_id, TaskState::Failed, Some(&err)).await?;
            state.stats.a2a_tasks_failed.fetch_add(1, Ordering::Relaxed);
            let mut t = initial_task(task_id, params.session_id, params.message);
            t.status = TaskStatus {
                state: TaskState::Failed,
                message: Some(text_message(&err)),
                timestamp: Utc::now(),
            };
            t.recursion_rounds = rounds;
            t.parent_task_id = params.parent_task_id;
            t
        }
    };

    // Emit final event (so SSE subscribers see end-of-stream).
    emit_event(state, task_id, "final", json!({"task": final_task})).await?;

    Ok(final_task)
}

/// Collapse a Vec<Part> down to its concatenated text body. Used by the
/// recursion loop to thread the previous round's output as conditioning.
fn parts_to_text(parts: &[Part]) -> String {
    let mut out = String::new();
    for p in parts {
        if let Part::Text { text, .. } = p {
            out.push_str(text);
            out.push('\n');
        }
    }
    out
}

/// Update `a2a_tasks.current_round` so streaming subscribers can observe
/// progress. Called once per completed round.
async fn set_current_round(state: &ApiState, task_id: Uuid, round: i32) -> Result<(), String> {
    let pool = state.db.pool().ok_or("no pool")?;
    sqlx::query("UPDATE a2a_tasks SET current_round = $1, updated_at = NOW() WHERE id = $2")
        .bind(round)
        .bind(task_id)
        .execute(pool)
        .await
        .map_err(|e| format!("update current_round: {}", e))?;
    Ok(())
}

/// Persist a per-round artifact. The `artifact_index` column reuses the
/// round number so we can recover the per-round transcript via
/// `SELECT * FROM a2a_artifacts WHERE task_id = $1 ORDER BY recursion_round`.
async fn insert_artifact_round(
    state: &ApiState,
    task_id: Uuid,
    round: i32,
    parts: Vec<Part>,
) -> Result<(), String> {
    let pool = state.db.pool().ok_or("no pool")?;
    let parts_json = serde_json::to_value(&parts).map_err(|e| e.to_string())?;
    sqlx::query(
        "INSERT INTO a2a_artifacts
            (task_id, parts, artifact_index, append, last_chunk, metadata, recursion_round)
         VALUES ($1, $2, $3, FALSE, TRUE, '{}'::jsonb, $4)",
    )
    .bind(task_id)
    .bind(&parts_json)
    .bind(round)
    .bind(round)
    .execute(pool)
    .await
    .map_err(|e| format!("insert artifact: {}", e))?;
    Ok(())
}

/// Fetch a Task's current state from Postgres.
pub async fn get_task(state: &ApiState, task_id: Uuid) -> Result<Option<Task>, String> {
    let pool = state.db.pool().ok_or("no pool")?;
    type TaskHeader = (
        String,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<String>,
        i32,
        i32,
        Option<Uuid>,
    );
    let row: Option<TaskHeader> = sqlx::query_as::<_, TaskHeader>(
        "SELECT status, updated_at, completed_at, error,
                recursion_rounds, current_round, parent_task_id
           FROM a2a_tasks WHERE id = $1",
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("task lookup: {}", e))?;
    let Some((
        status_str,
        updated_at,
        _completed_at,
        error,
        recursion_rounds,
        current_round,
        parent_task_id,
    )) = row
    else {
        return Ok(None);
    };
    let state_enum = TaskState::from_db_str(&status_str);

    // Reconstruct history.
    let messages: Vec<(String, serde_json::Value, i32)> =
        sqlx::query_as::<_, (String, serde_json::Value, i32)>(
            "SELECT role, parts, sequence FROM a2a_messages WHERE task_id = $1 ORDER BY sequence",
        )
        .bind(task_id)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("history: {}", e))?;
    let history: Vec<Message> = messages
        .into_iter()
        .map(|(role, parts, _)| Message {
            role: Role::from_db_str(&role),
            parts: serde_json::from_value(parts).unwrap_or_default(),
        })
        .collect();

    // Artifacts.
    let arts: Vec<(
        Option<String>,
        serde_json::Value,
        i32,
        bool,
        bool,
        serde_json::Value,
    )> = sqlx::query_as::<
        _,
        (
            Option<String>,
            serde_json::Value,
            i32,
            bool,
            bool,
            serde_json::Value,
        ),
    >(
        "SELECT name, parts, artifact_index, append, last_chunk, metadata
             FROM a2a_artifacts WHERE task_id = $1 ORDER BY artifact_index",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("artifacts: {}", e))?;
    let artifacts: Vec<Artifact> = arts
        .into_iter()
        .map(
            |(name, parts, index, append, last_chunk, metadata)| Artifact {
                name,
                parts: serde_json::from_value(parts).unwrap_or_default(),
                index,
                append,
                last_chunk,
                metadata,
            },
        )
        .collect();

    Ok(Some(Task {
        id: task_id,
        session_id: None,
        status: TaskStatus {
            state: state_enum,
            message: error.map(|e| text_message(&e)),
            timestamp: updated_at.unwrap_or_else(Utc::now),
        },
        history: if history.is_empty() {
            None
        } else {
            Some(history)
        },
        artifacts,
        metadata: serde_json::Value::Null,
        recursion_rounds: recursion_rounds.max(1) as u32,
        current_round: current_round.max(0) as u32,
        parent_task_id,
    }))
}

/// Cancel a Task. Idempotent — cancelling a terminal Task is a no-op
/// (returns the existing terminal state).
pub async fn cancel_task(state: &ApiState, task_id: Uuid) -> Result<Task, String> {
    let pool = state.db.pool().ok_or("no pool")?;
    let cur: Option<(String,)> =
        sqlx::query_as::<_, (String,)>("SELECT status FROM a2a_tasks WHERE id = $1")
            .bind(task_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("task lookup: {}", e))?;
    let Some((status_str,)) = cur else {
        return Err("Task not found".into());
    };
    let current = TaskState::from_db_str(&status_str);
    if !current.is_terminal() {
        set_state(state, task_id, TaskState::Canceled, None).await?;
        state
            .stats
            .a2a_tasks_canceled
            .fetch_add(1, Ordering::Relaxed);
        emit_event(state, task_id, "status", json!({"status": "canceled"})).await?;
    }
    get_task(state, task_id)
        .await?
        .ok_or_else(|| "Task vanished".into())
}

/// Register a push-notification URL on a Task.
pub async fn set_push_notification(
    state: &ApiState,
    task_id: Uuid,
    url: &str,
) -> Result<(), String> {
    let pool = state.db.pool().ok_or("no pool")?;
    sqlx::query(
        "UPDATE a2a_tasks SET push_notification_url = $1, updated_at = NOW() WHERE id = $2",
    )
    .bind(url)
    .bind(task_id)
    .execute(pool)
    .await
    .map_err(|e| format!("push set: {}", e))?;
    Ok(())
}

pub async fn get_push_notification(
    state: &ApiState,
    task_id: Uuid,
) -> Result<Option<String>, String> {
    let pool = state.db.pool().ok_or("no pool")?;
    let row: Option<(Option<String>,)> = sqlx::query_as::<_, (Option<String>,)>(
        "SELECT push_notification_url FROM a2a_tasks WHERE id = $1",
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("push get: {}", e))?;
    Ok(row.and_then(|r| r.0))
}

async fn set_state(
    state: &ApiState,
    task_id: Uuid,
    new_state: TaskState,
    err: Option<&str>,
) -> Result<(), String> {
    let pool = state.db.pool().ok_or("no pool")?;
    let is_terminal = new_state.is_terminal();
    sqlx::query(
        "UPDATE a2a_tasks
            SET status = $1,
                updated_at = NOW(),
                completed_at = CASE WHEN $3 THEN NOW() ELSE completed_at END,
                error = COALESCE($4, error)
            WHERE id = $2",
    )
    .bind(new_state.as_db_str())
    .bind(task_id)
    .bind(is_terminal)
    .bind(err)
    .execute(pool)
    .await
    .map_err(|e| format!("set_state: {}", e))?;
    emit_event(
        state,
        task_id,
        "status",
        json!({"state": new_state.as_db_str()}),
    )
    .await?;
    Ok(())
}

async fn emit_event(
    state: &ApiState,
    task_id: Uuid,
    kind: &str,
    payload: serde_json::Value,
) -> Result<(), String> {
    let pool = state.db.pool().ok_or("no pool")?;
    // Next sequence per task.
    let seq: (i32,) = sqlx::query_as::<_, (i32,)>(
        "SELECT COALESCE(MAX(sequence), -1) + 1 FROM a2a_events WHERE task_id = $1",
    )
    .bind(task_id)
    .fetch_one(pool)
    .await
    .map_err(|e| format!("seq: {}", e))?;
    sqlx::query(
        "INSERT INTO a2a_events (task_id, kind, payload, sequence) VALUES ($1, $2, $3, $4)",
    )
    .bind(task_id)
    .bind(kind)
    .bind(&payload)
    .bind(seq.0)
    .execute(pool)
    .await
    .map_err(|e| format!("insert event: {}", e))?;
    state
        .stats
        .a2a_events_emitted
        .fetch_add(1, Ordering::Relaxed);
    // pg_notify is best-effort — failures are non-fatal.
    let _ = sse::notify_task(pool, task_id, kind, &payload).await;
    Ok(())
}

async fn insert_artifact(state: &ApiState, task_id: Uuid, parts: Vec<Part>) -> Result<(), String> {
    let pool = state.db.pool().ok_or("no pool")?;
    let parts_json = serde_json::to_value(&parts).map_err(|e| e.to_string())?;
    sqlx::query(
        "INSERT INTO a2a_artifacts (task_id, parts, artifact_index, append, last_chunk, metadata)
         VALUES ($1, $2, 0, FALSE, TRUE, '{}'::jsonb)",
    )
    .bind(task_id)
    .bind(&parts_json)
    .execute(pool)
    .await
    .map_err(|e| format!("insert artifact: {}", e))?;
    Ok(())
}

fn extract_text(msg: &Message) -> String {
    let mut out = String::new();
    for p in &msg.parts {
        if let Part::Text { text, .. } = p {
            out.push_str(text);
            out.push('\n');
        }
    }
    out
}

/// Invoke the named skill / MCP tool. Returns the result as one or more
/// `Part`s ready to attach as an Artifact.
async fn invoke_skill(
    state: &ApiState,
    task_id: Uuid,
    skill_id: &str,
    request_text: &str,
    _original_msg: &Message,
) -> Result<Vec<Part>, String> {
    // For umbrella skills we route through `orient` to pick the best tool;
    // for direct tool names we invoke the tool directly.
    let (tool_name, args) = if skill_id == "code-analysis" {
        ("orient", json!({"prompt": request_text}))
    } else {
        // Map skill to a tool. For request_text-driven invocation, pass the
        // text in a generic `prompt` argument plus any structured JSON
        // payload from the original message's first Data part.
        (skill_id, json!({"prompt": request_text}))
    };

    // We invoke via a fresh McpServer constructed against the same SystemContext.
    // The call_tool_cli path is the canonical dispatch the test harness uses.
    let server = crate::mcp::server::McpServer::new(state.system_ctx.clone());

    emit_event(
        state,
        task_id,
        "message",
        json!({"role": "agent", "tool": tool_name, "args": args}),
    )
    .await?;

    let res = server
        .call_tool_cli(tool_name, args)
        .await
        .map_err(|e| format!("tool error: {:?}", e))?;

    if res.is_error == Some(true) {
        return Err("MCP tool returned an error".into());
    }

    // Translate CallToolResult content to A2A Parts.
    let mut parts: Vec<Part> = Vec::new();
    for content in &res.content {
        if let Some(text) = content.as_text().map(|t| t.text.clone()) {
            parts.push(Part::Text {
                text,
                metadata: serde_json::Value::Null,
            });
        }
    }
    if parts.is_empty() {
        parts.push(Part::Text {
            text: "(no content returned)".into(),
            metadata: serde_json::Value::Null,
        });
    }
    Ok(parts)
}

/// POST the final Task JSON to a registered webhook URL.
async fn fire_push_notification(url: &str, task: &Task) -> Result<(), String> {
    let body = serde_json::to_string(task).map_err(|e| e.to_string())?;
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?
        .post(url)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("push status {}", resp.status()));
    }
    Ok(())
}
