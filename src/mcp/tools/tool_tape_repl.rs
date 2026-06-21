//! `tape_repl` — run a **sandboxed white-box REPL** script against a recursion
//! tree's tape, behind a structural admission gate.
//!
//! This is the *white-box / latent-tier* counterpart of the nine black-box-legal
//! tape verbs. It scripts the tape through context-tape's deny-by-default `rhai`
//! engine (the nine verbs only; no filesystem/network/process package; `eval`
//! disabled) under hard, deterministic limits. The engine, the host verb surface,
//! and every bound live in **context-tape** — pgmcp never executes agent code
//! itself; it only *admits or refuses* the run and lends the per-tree store to the
//! synchronous engine.
//!
//! ## Admission (the gate)
//!
//! [`repl_host::repl_admitted`] admits the run **iff both**:
//!   1. the caller is a **white-box** role (a black-box agent — Claude, Codex — is
//!      structurally refused on the `Latent` REPL edge via the CSM media
//!      discipline; white-box status is a host-side fact, never a self-reported
//!      claim), AND
//!   2. the named `experiment_slug` resolves to an **Open** experiment (DB-backed).
//!
//! On refusal the body returns `{ admitted: false, reason }` (the `warn!` is
//! emitted inside [`repl_host::repl_admitted`], ADR-021 trust-boundary-refused).
//! On admission it returns `{ admitted: true, value, value_type, pages_touched,
//! bytes_touched, ops, over_limit, limit, error }`; a budget exhaustion is a
//! structured `over_limit: true`, never a 500.
//!
//! ## Trust model — the caller role is the host-extracted transport identity
//!
//! The role-trust input is the caller's **transport identity**, derived host-side
//! from the MCP `initialize` handshake (`clientInfo.name`, lowercased) by
//! [`extract_caller`](crate::mcp::server::extract_caller) — *never* read from the
//! request payload. The MCP wire handler ([`tape_repl`](crate::mcp::server)) threads
//! that identity into [`tool_tape_repl_with_caller`]; [`repl_host::caller_role`]
//! maps it onto the [`Role`] the gate evaluates against the canonical
//! [`repl_host::black_box_roles`] registry. This is the surface-side half of the
//! defect fix: the gate's logic was always correct, but the body formerly handed it
//! a single constant white-box role for *every* caller, so the black-box refusal
//! could never fire from the wire.
//!
//! **Fail-closed on an unidentified caller.** A caller pgmcp cannot positively
//! identify — an empty / `"unknown"` name (the peer never completed `initialize`),
//! or the `"cli"` dispatch path (which carries no per-call transport identity) — is
//! mapped by [`repl_host::caller_role`] onto a canonical black-box role and is
//! therefore **refused**. The 2-argument [`tool_tape_repl`] entry (the
//! `call_tool_cli` dispatch path) supplies `None` for the caller and so fails
//! closed by construction; only the wire handler, which has a `RequestContext`,
//! can supply a positively-identified white-box backbone.
//!
//! **Why a self-reported white-box claim cannot pass.** The body reads no
//! caller-supplied "I am white-box" field; the REPL edge's medium is a constant of
//! the capability (`Latent`); and the only role-trust signal is the host-extracted
//! identity compared against the host-side black-box set. The *live* admit/refuse
//! decision is then further gated by the DB-backed experiment status (Open ⇒
//! admit; anything else, or unverifiable ⇒ refuse).
//!
//! Boundary: analytical host wrapper — no shell/exec in pgmcp; the durable corpus
//! is never written (the REPL's `put` is `Scratch`-only); never writes the user's
//! source files.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::db::queries;
use crate::experiment::vocab::ExperimentStatus;
use crate::mcp::server::TapeReplParams;
use crate::mcp::tools::sota_helpers::json_result;
use crate::mcp::tools::tape_support::tree_id_of;
use crate::tape::repl_host::{self, ReplAdmission};

/// `tape_repl` body for the **`call_tool_cli` dispatch path** (no `RequestContext`,
/// hence no per-call transport identity).
///
/// This is the 2-argument entry the `dispatch_tool!` macro invokes. It carries no
/// caller identity, so it **fails closed**: it forwards `None` to
/// [`tool_tape_repl_with_caller`], which maps an unidentified caller onto a
/// black-box role and refuses on the latent REPL edge. The MCP wire handler, which
/// *does* have a `RequestContext`, calls [`tool_tape_repl_with_caller`] directly
/// with the host-extracted identity.
pub async fn tool_tape_repl(
    ctx: &SystemContext,
    params: TapeReplParams,
) -> Result<CallToolResult, McpError> {
    tool_tape_repl_with_caller(ctx, params, None).await
}

/// `tape_repl` body, parameterized by the **host-extracted caller identity**.
///
/// `caller_identity` is the lowercased MCP `clientInfo.name` from
/// [`extract_caller`](crate::mcp::server::extract_caller) on the wire path, or
/// `None` on the `call_tool_cli` path. It is mapped through
/// [`repl_host::caller_role`] (fail-closed: an unidentified or known-black-box
/// identity is refused on the latent edge) before the admission gate runs.
pub async fn tool_tape_repl_with_caller(
    ctx: &SystemContext,
    params: TapeReplParams,
    caller_identity: Option<&str>,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    let tree_id = tree_id_of(&params.tree);

    // --- Resolve the experiment status (DB-backed, trustworthy). ---
    // The gate is pure/synchronous, so the (async) DB fetch happens here and the
    // already-resolved status is handed to `repl_admitted`. When no real pool is
    // available (CLI / mock-DB mode) the experiment cannot be confirmed Open, so
    // admission fails closed (an unverifiable experiment is not an Open one) — a
    // by-design refusal, not a swallowed error.
    let experiment_status: Option<ExperimentStatus> = match ctx.db().pool() {
        Some(pool) => {
            match queries::get_experiment_core(pool, None, Some(&params.experiment_slug)).await {
                Ok(Some(core)) => ExperimentStatus::parse(&core.status),
                Ok(None) => None,
                Err(e) => {
                    // A genuine DB failure (not a by-design refusal) → ADR-021 `error!`.
                    tracing::error!(
                        slug = %params.experiment_slug,
                        "tape_repl: get_experiment_core failed: {e}"
                    );
                    return Err(McpError::internal_error(
                        format!("tape_repl: experiment lookup failed: {e}"),
                        None,
                    ));
                }
            }
        }
        None => None,
    };

    // --- The admission gate (pure, structural + the resolved experiment status). ---
    // The caller role is the host-extracted transport identity (NOT a payload
    // field), mapped fail-closed: an empty / "unknown" / "cli" identity becomes a
    // black-box role and is refused on the latent REPL edge, as is a known
    // black-box agent ("claude" / "codex"). Both the role and the comparison set
    // are lowercase so the wire identity from `extract_caller` actually matches.
    let caller = repl_host::caller_role(caller_identity);
    let admission =
        repl_host::repl_admitted(&caller, &repl_host::black_box_roles(), experiment_status);

    match admission {
        ReplAdmission::Refused { reason } => {
            // The `warn!` was emitted inside `repl_admitted` (trust-boundary refused).
            json_result(&json!({
                "tree": params.tree,
                "admitted": false,
                "reason": reason,
            }))
        }
        ReplAdmission::Admitted => {
            let limits = params.limits.unwrap_or_default().into_limits();
            // Run the synchronous engine inside `with_store_mut`; no `&mut TapeStore`
            // crosses an await point (see `repl_host::run_repl`).
            let result = repl_host::run_repl(ctx, tree_id, &params.script, limits);
            json_result(&json!({
                "tree": params.tree,
                "admitted": true,
                "value": result.value,
                "value_type": result.value_type,
                "pages_touched": result.pages_touched,
                "bytes_touched": result.bytes_touched,
                "ops": result.ops,
                "over_limit": result.over_limit,
                "limit": result.limit,
                "error": result.error,
            }))
        }
    }
}
