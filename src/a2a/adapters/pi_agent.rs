//! Adapter: expose the `pi` coding agent as an A2A peer (Crucible E2).

#![allow(dead_code)]

use super::generic_subprocess::GenericSubprocessAdapter;

#[derive(Clone)]
pub struct PiAdapter {
    pub inner: GenericSubprocessAdapter,
}

impl PiAdapter {
    /// Build a single-shot, MCP-free `pi` leaf pinned to one provider/model.
    ///
    /// Anti-recursion posture: pi's *core* has NO MCP (MCP is an opt-in extension),
    /// so a stock `pi -p --no-builtin-tools` leaf cannot reconnect to the daemon and
    /// re-enter the `a2a_pattern_*` tools — structurally preventing unbounded
    /// cross-agent recursion, matching the adapter's "stateless leaf (string in →
    /// string out)" contract (the same guarantee the Claude adapter gets from
    /// `--strict-mcp-config`, but here it is the pi default). `--no-builtin-tools`
    /// also removes file/bash tools, so the leaf is pure reasoning. `--provider` /
    /// `--model` pin one backing model so a single `pi` binary can back many role
    /// peers. If you have installed an MCP extension globally, run leaves with an
    /// extension-free pi config (or override the argv via `with_args`) so the
    /// no-MCP guarantee holds.
    pub fn new(provider: Option<String>, model: Option<String>) -> Self {
        let mut args = vec!["-p".to_string(), "--no-builtin-tools".to_string()];
        if let Some(p) = provider {
            args.push("--provider".to_string());
            args.push(p);
        }
        if let Some(m) = model {
            args.push("--model".to_string());
            args.push(m);
        }
        args.push("{{message}}".to_string());
        Self {
            inner: GenericSubprocessAdapter::new("pi", args),
        }
    }

    /// Full argv override (with a `{{message}}` placeholder), for non-stock pi
    /// setups that need extra flags.
    pub fn with_args(args: Vec<String>) -> Self {
        Self {
            inner: GenericSubprocessAdapter::new("pi", args),
        }
    }

    pub async fn execute(&self, message: &str) -> Result<String, String> {
        self.inner.execute(message).await
    }
}

impl Default for PiAdapter {
    fn default() -> Self {
        Self::new(None, None)
    }
}
