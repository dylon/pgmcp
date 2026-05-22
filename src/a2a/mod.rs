//! Agent-to-Agent (A2A) protocol surface.
//!
//! Implements a substantive subset of Google's A2A spec
//! (https://google.github.io/A2A/) on the pgmcp daemon so external agents
//! (Claude Code, Codex CLI, OpenAI Agents SDK, etc.) can discover the
//! daemon's capabilities, submit Tasks, stream events, push notifications,
//! and dispatch out to peer agents.
//!
//! Modules:
//!
//! - `types`: wire-level structs mirroring the A2A spec.
//! - `skills`: static + auto-enumerated Skill list for the AgentCard.
//! - `server`: axum routes for `/.well-known/agent.json`, `/a2a/jsonrpc`, `/a2a/sse/{task_id}`, `/a2a/agents`.
//! - `handlers`: JSON-RPC dispatch for `tasks/send`, `tasks/get`, etc.
//! - `dispatcher`: worker that takes Tasks, invokes MCP tools, writes events.
//! - `client`: outbound A2A client for talking to peer agents.
//! - `sse`: bridge for streaming task events.
//! - `adapters`: non-A2A CLI wrappers (claude-code, codex-cli, generic).

#![allow(dead_code)] // Wired up incrementally as the daemon picks up A2A bits.
#![allow(unused_imports)] // Public re-exports for external consumers (tests, examples).

pub mod adapters;
pub mod client;
pub mod dispatcher;
pub mod handlers;
pub mod server;
pub mod skills;
pub mod sse;
pub mod types;

pub use server::a2a_router;
pub use types::{AgentCard, AgentSkill, Artifact, Event, Message, Part, Task, TaskState};
