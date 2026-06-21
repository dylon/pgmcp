//! `protocol_soundness` — in-process deadlock-freedom + progress for a CSM
//! `GlobalType` (Task #22 §4-A).
//!
//! No subprocess, no prattail, no external model checker. By the proofs-as-plans
//! result (`CsmDeadlockFreedom.v`, §4-D): a **well-formed** `GlobalType` is
//! deadlock-free and has progress *by typing* (the session-types-as-linear-logic
//! correspondence, Caires–Pfenning / Wadler). So soundness reduces to MPST
//! well-formedness — closing the ADR-012 gap that otherwise needs pi + TLC.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::csm::mpst::global::GlobalType;
use crate::csm::mpst::wellformed::well_formed;
use crate::mcp::server::ProtocolSoundnessParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn tool_protocol_soundness(
    _ctx: &SystemContext,
    params: ProtocolSoundnessParams,
) -> Result<CallToolResult, McpError> {
    let g: GlobalType = serde_json::from_value(params.global_type)
        .map_err(|e| McpError::invalid_params(format!("invalid GlobalType JSON: {e}"), None))?;

    let wf = well_formed(&g);
    let (deadlock_free, error) = match &wf {
        Ok(()) => (true, None),
        Err(e) => (false, Some(format!("{e:?}"))),
    };

    json_result(&json!({
        "well_formed": wf.is_ok(),
        // By CsmDeadlockFreedom.v these are *implied by* well-formedness, not
        // independently model-checked — the plan is correct by construction.
        "deadlock_free": deadlock_free,
        "has_progress": deadlock_free,
        "error": error,
        "method": "mpst-wellformedness",
        "certificate":
            "CsmDeadlockFreedom.v: well_formed(g) ⇒ deadlock_free(g) ∧ progress(g) \
             by typing (Caires–Pfenning 2010 / Wadler 2014); admission-free Rocq.",
    }))
}
