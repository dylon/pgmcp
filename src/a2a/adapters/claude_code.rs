//! Adapter: expose the `claude` CLI as an A2A peer.

#![allow(dead_code)]

use super::generic_subprocess::GenericSubprocessAdapter;

#[derive(Clone)]
pub struct ClaudeCodeAdapter {
    pub inner: GenericSubprocessAdapter,
}

impl ClaudeCodeAdapter {
    /// Build with default args. Customize via `with_args` if needed.
    pub fn new() -> Self {
        Self {
            inner: GenericSubprocessAdapter::new("claude", vec!["-p".into(), "{{message}}".into()]),
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
