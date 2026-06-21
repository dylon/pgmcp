//! Parameters for the native formal-verification (FV) MCP tools (Task #22 §4-A).
//!
//! Each tool runs `{pgmcp data | inline spec} → lling-llang/CSM engine → verdict`
//! entirely in-process (no subprocess, no prattail dependency). Param structs are
//! glob-re-exported by `params/mod.rs` so `crate::mcp::server::<Name>Params` resolves.

use rmcp::schemars;
use serde::Deserialize;

/// `protocol_soundness` — deadlock-freedom + progress for a CSM `GlobalType`.
///
/// By the proofs-as-plans result (`CsmDeadlockFreedom.v`, Task #22 §4-D), a
/// **well-formed** `GlobalType` is deadlock-free and has progress *by typing*, so this
/// tool decides soundness by checking MPST well-formedness — closing the gap that
/// otherwise needs an external model checker (pi + TLC).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProtocolSoundnessParams {
    /// The global protocol type as adjacent-tagged JSON (`{"type": …, "data": …}`),
    /// matching `csm::mpst::global::GlobalType`.
    #[schemars(description = "GlobalType as adjacent-tagged JSON ({\"type\":…,\"data\":…})")]
    pub global_type: serde_json::Value,
}
