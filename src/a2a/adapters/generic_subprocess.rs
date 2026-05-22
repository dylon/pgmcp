//! Generic CLI adapter: spawns an arbitrary subprocess to execute Tasks.
//!
//! The adapter takes a command template like `gpt --prompt {{message}}` and
//! substitutes the user message text at call time. stdout is captured and
//! returned as a text Artifact.

#![allow(dead_code)]

use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Clone)]
pub struct GenericSubprocessAdapter {
    pub command: String,
    pub args_template: Vec<String>,
    pub timeout: Duration,
}

impl GenericSubprocessAdapter {
    pub fn new(command: impl Into<String>, args_template: Vec<String>) -> Self {
        Self {
            command: command.into(),
            args_template,
            timeout: Duration::from_secs(120),
        }
    }

    /// Substitute `{{message}}` placeholders in args_template with the
    /// caller-provided text, then spawn the subprocess. Returns the
    /// captured stdout as a UTF-8 string (best-effort).
    pub async fn execute(&self, message_text: &str) -> Result<String, String> {
        let mut args: Vec<String> = Vec::with_capacity(self.args_template.len());
        for a in &self.args_template {
            args.push(a.replace("{{message}}", message_text));
        }
        let mut child = Command::new(&self.command)
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn {}: {}", self.command, e))?;
        let stdout = child.stdout.take().ok_or("no stdout pipe")?;
        let mut reader = tokio::io::BufReader::new(stdout);
        let mut out = String::new();
        let read_fut = async {
            reader
                .read_to_string(&mut out)
                .await
                .map_err(|e| e.to_string())?;
            child.wait().await.map_err(|e| e.to_string())?;
            Ok::<_, String>(())
        };
        timeout(self.timeout, read_fut)
            .await
            .map_err(|_| "timed out")??;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn echo_subprocess_returns_stdout() {
        let adapter =
            GenericSubprocessAdapter::new("/bin/sh", vec!["-c".into(), "echo {{message}}".into()]);
        let out = adapter.execute("hello world").await.expect("echo");
        assert!(out.contains("hello world"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn template_substitution_replaces_placeholder() {
        let adapter =
            GenericSubprocessAdapter::new("/bin/sh", vec!["-c".into(), "echo {{message}}".into()]);
        let out = adapter.execute("integration-test").await.expect("echo");
        assert!(out.contains("integration-test"));
    }
}
