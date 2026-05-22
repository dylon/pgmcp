//! Adapter: expose OpenAI's Codex CLI as an A2A peer.

#![allow(dead_code)]

use super::generic_subprocess::GenericSubprocessAdapter;

#[derive(Clone)]
pub struct CodexCliAdapter {
    pub inner: GenericSubprocessAdapter,
}

impl CodexCliAdapter {
    pub fn new() -> Self {
        Self {
            inner: GenericSubprocessAdapter::new("codex", vec!["{{message}}".into()]),
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
