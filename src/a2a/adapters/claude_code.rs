//! Adapter: expose the `claude` CLI as an A2A peer.

#![allow(dead_code)]

use super::generic_subprocess::GenericSubprocessAdapter;

#[derive(Clone)]
pub struct ClaudeCodeAdapter {
    pub inner: GenericSubprocessAdapter,
}

impl ClaudeCodeAdapter {
    /// Build with default args. The leaf runs with pgmcp's MCP server DISABLED
    /// (`--strict-mcp-config --mcp-config '{"mcpServers":{}}'` — verified Claude
    /// Code flags: "only use MCP servers from --mcp-config, ignoring all other
    /// MCP configurations") so a spawned `claude -p` leaf cannot reconnect to
    /// the daemon and re-enter the `a2a_pattern_*` tools. This structurally
    /// prevents unbounded cross-agent recursion and matches the adapter's
    /// "stateless leaf (string in → string out)" contract. Customize via
    /// `with_args` if needed.
    pub fn new() -> Self {
        Self {
            inner: GenericSubprocessAdapter::new(
                "claude",
                vec![
                    "-p".into(),
                    "--strict-mcp-config".into(),
                    "--mcp-config".into(),
                    r#"{"mcpServers":{}}"#.into(),
                    "{{message}}".into(),
                ],
            ),
        }
    }

    pub fn with_args(args: Vec<String>) -> Self {
        Self {
            inner: GenericSubprocessAdapter::new("claude", args),
        }
    }

    pub async fn execute(&self, message: &str) -> Result<String, String> {
        self.inner.execute(message).await
    }
}

impl Default for ClaudeCodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}
