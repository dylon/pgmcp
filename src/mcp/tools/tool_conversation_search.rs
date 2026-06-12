//! `tool_conversation_search` — v31 convenience wrapper over
//! `memory_unified_search`.
//!
//! Pins `node_types` to the agent-to-agent conversation family
//! (`a2a_message`, `agent_message`, `a2a_task`, `coordination_request`) so a
//! caller can search "what did the agents discuss / negotiate about X" without
//! having to remember the node-type vocabulary. Retrieval, embedding, validation
//! and the result envelope are entirely delegated to `tool_memory_unified_search`
//! (single source of truth); this is purely a node-type-narrowing facade.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use crate::context::SystemContext;
use crate::mcp::server::{ConversationSearchParams, MemoryUnifiedSearchParams};

/// The conversation node-type family this tool searches. `a2a_task` is a hub
/// node (no embedding) — it is included so the result can surface a task even
/// when only its transcript messages are vector-similar (the unified search
/// itself only vector-seeds embedded nodes, but the pin documents intent and
/// keeps the family stable if the hub later gains an embedding).
const CONVERSATION_NODE_TYPES: &[&str] = &[
    "a2a_message",
    "agent_message",
    "a2a_task",
    "coordination_request",
];

pub async fn tool_conversation_search(
    ctx: &SystemContext,
    params: ConversationSearchParams,
) -> Result<CallToolResult, McpError> {
    let node_types = CONVERSATION_NODE_TYPES
        .iter()
        .map(|s| s.to_string())
        .collect();
    crate::mcp::tools::tool_memory_graph_rag::tool_memory_unified_search(
        ctx,
        MemoryUnifiedSearchParams {
            query: params.query,
            node_types: Some(node_types),
            k: params.k,
        },
    )
    .await
}
