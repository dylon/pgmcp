//! Adapter: expose OpenAI's Codex CLI as an A2A peer.

#![allow(dead_code)]

use super::generic_subprocess::GenericSubprocessAdapter;

#[derive(Clone)]
pub struct CodexCliAdapter {
    pub inner: GenericSubprocessAdapter,
}

impl CodexCliAdapter {
    /// The leaf runs with MCP servers DISABLED (`-c mcp_servers={}` — Codex's
    /// verified config-override flag, clearing the `[mcp_servers]` table) so a
    /// spawned `codex` leaf cannot reconnect to the daemon and re-enter the
    /// `a2a_pattern_*` tools — structurally preventing unbounded cross-agent
    /// recursion. Matches the adapter's "stateless leaf" contract.
    pub fn new() -> Self {
        Self {
            inner: GenericSubprocessAdapter::new(
                "codex",
                vec!["-c".into(), "mcp_servers={}".into(), "{{message}}".into()],
            ),
        }
    }

    pub fn with_args(args: Vec<String>) -> Self {
        Self {
            inner: GenericSubprocessAdapter::new("codex", args),
        }
    }

    pub async fn execute(&self, message: &str) -> Result<String, String> {
        self.inner.execute(message).await
    }
}

impl Default for CodexCliAdapter {
    fn default() -> Self {
        Self::new()
    }
}
