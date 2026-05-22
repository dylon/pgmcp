//! Adapter shims that expose non-A2A-native CLIs as A2A peers.
//!
//! Each adapter is a long-running process that listens on its own A2A port
//! and translates incoming Tasks into a CLI subprocess invocation. The CLI's
//! stdout is streamed back as A2A Events / Artifacts.

#![allow(unused_imports)]

pub mod claude_code;
pub mod codex_cli;
pub mod generic_subprocess;

pub use claude_code::ClaudeCodeAdapter;
pub use codex_cli::CodexCliAdapter;
pub use generic_subprocess::GenericSubprocessAdapter;
