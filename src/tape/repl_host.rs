//! The **`tape_repl` admission gate** + the host glue that runs the
//! context-tape sandboxed white-box REPL ([`context_tape::repl::ReplEngine`])
//! against a per-tree [`context_tape::TapeStore`].
//!
//! pgmcp stays *analytical and communicative*: it never executes agent code
//! itself. The `rhai` engine, the nine-verb [`TapeApi`](context_tape::repl) host
//! surface, and every resource bound live in **context-tape**, behind a
//! deny-by-default sandbox (no filesystem/network/process package; `eval`
//! disabled; corpus read-only — `put` is `Scratch`-only). This module is the
//! thin pgmcp-side wrapper that (1) decides *whether the REPL is admitted at all*
//! and (2) lends the per-tree store to the synchronous engine for exactly the
//! span of one run. Nothing here reads or writes the user's source files, and
//! the durable corpus is never written.
//!
//! ## The admission gate ([`repl_admitted`]) — structural trust, no self-cert
//!
//! The plan's architecture names this `TapeController::repl_admitted`, but pgmcp
//! has **no `TapeController` trait today** (confirmed by grep). Per Occam we
//! implement it as a free function rather than introduce an otherwise-unused
//! trait; this *is* the body a future `TapeController::repl_admitted` would
//! delegate to. The REPL is admitted **iff both** of two *trustworthy*
//! conditions hold — neither of which an agent can fabricate from its own
//! request payload:
//!
//! 1. **White-box medium (structural).** The REPL is the *white-box / latent
//!    tier* capability — the counterpart of the latent RecursiveMAS edge, not the
//!    all-Text black-box-legal tape verbs. The medium of the edge over which the
//!    REPL is reached is therefore **`Latent`**, fixed by the capability as an
//!    internal constant ([`REPL_EDGE_LABEL`]); it is *never* taken from the
//!    caller. We reuse the existing projection-time discipline
//!    ([`check_media_discipline`](crate::csm::media::check_media_discipline)):
//!    model the REPL as the one-edge global type `O → caller : repl_dsl (Latent)`
//!    and run the discipline against the trustworthy black-box role set. A
//!    **black-box** caller (an agent with no hidden-state access — Claude Code,
//!    Codex; ADR-009) on a `Latent` edge is a discipline violation
//!    ([`MediaError::BlackBoxOnLatent`](crate::csm::media::MediaError)) → refused.
//!    Only a *white-box backbone* role (absent from the black-box set) survives.
//!
//!    **Why a self-reported white-box claim cannot pass.** Admission does not
//!    read any caller-supplied "I am white-box" field. The only role-trust input
//!    is membership in `black_box`, which is a **host-side structural fact** (the
//!    registry of agents known to have no hidden state), not a claim carried on
//!    the wire — exactly pgmcp's existing structural-trust boundary, where an
//!    agent can self-*report* but never self-*certify* (cf. the tracker's missing
//!    `Agent` judgment arm). The REPL edge's medium is likewise a constant of the
//!    capability, so a caller cannot relabel the edge `Text` to launder itself
//!    past the latent-tier gate.
//!
//!    **The caller role is the host-extracted transport identity, fail-closed.**
//!    The MCP wire handler derives the caller from the `initialize`-handshake
//!    `clientInfo.name` (via `extract_caller`, lowercased) — NOT from the request
//!    payload — and hands it to the tool body, which maps it through
//!    [`caller_role`]. A *known black-box* identity (`claude` / `codex`, the
//!    [`black_box_roles`] registry) becomes that black-box role and is refused on
//!    the latent edge. An **unidentified** caller — an empty / `"unknown"` name
//!    (the peer has not completed `initialize`) or the `"cli"` dispatch path,
//!    which carries no per-call transport identity — is treated as black-box
//!    **by construction** ([`caller_role`] maps it onto the canonical black-box
//!    role), so the gate fails *closed*: an unidentified caller can NEVER ride the
//!    latent REPL edge. Only a positively-identified white-box backbone role
//!    (absent from the black-box set) survives the medium arm.
//!
//! 2. **Open experiment (DB-backed).** The named experiment slug must resolve to
//!    a row whose status is [`ExperimentStatus::Open`](crate::experiment::vocab::ExperimentStatus).
//!    This couples a live REPL to a *declared, open* scientific experiment
//!    without hard-coding P9's specific experiment. The status is resolved from
//!    Postgres by the async caller and passed in **already resolved**, keeping
//!    [`repl_admitted`] a pure, synchronous, DB-free function (so it is exhaustively
//!    unit-testable without a database).
//!
//! A refusal returns [`ReplAdmission::Refused`] and is logged at **`warn!`** —
//! the ADR-021 *trust-boundary "refused"* category (an expected, by-design
//! denial, not a swallowed runtime error). DB/IO failures encountered while
//! *resolving* admission inputs are logged at `error!` by the async caller.

use std::collections::BTreeSet;

use context_tape::TreeId;
use context_tape::repl::{ReplEngine, ReplError, ReplLimits};

use crate::context::SystemContext;
use crate::csm::media::check_media_discipline;
use crate::csm::mpst::global::{end, interaction};
use crate::csm::role::{Label, Role};
use crate::experiment::vocab::ExperimentStatus;

/// The (constant) label of the REPL's protocol edge. The REPL is the white-box /
/// latent tier, so its edge medium is **`Latent`** — fixed by the capability,
/// never caller-supplied. The `hidden_size` / backbone signature are nominal:
/// the discipline only inspects `Label::is_latent()`, so their exact values do
/// not affect admission. We pin them to the ADR-009 RecursiveMAS defaults for
/// readability.
fn repl_edge_label() -> Label {
    Label::latent("repl_dsl", 4096, "qwen3-8b")
}

/// The orchestrator role that *offers* the REPL edge. Naming it `O` mirrors the
/// CSM protocol literals (`O` = Orchestrator) and the existing media tests; it is
/// never black-box (the orchestrator owns the white-box backbone), so the
/// discipline outcome is governed entirely by the *caller* role's black-box
/// membership.
const REPL_OFFERER: &str = "O";

/// The **canonical** structural registry of black-box agent roles (ADR-009 —
/// agents with no hidden-state access). A caller whose role is in this set is
/// refused on the `Latent` REPL edge by the media discipline.
///
/// The identities are **lowercase** so they compare equal to the caller role
/// derived from [`extract_caller`](crate::mcp::server::extract_caller), which
/// lowercases the MCP `clientInfo.name` (it yields e.g. `"claude"`, never
/// `"Claude"`). This casing is the integrity hinge: a TitleCase set here would
/// never match the lowercased transport identity, so the gate would silently
/// fail *open*. Keep this set lowercase, and pass the caller in the same casing.
pub fn black_box_roles() -> BTreeSet<Role> {
    ["claude", "codex"].iter().map(|r| Role::new(*r)).collect()
}

/// Map a host-extracted **transport identity** (the lowercased MCP
/// `clientInfo.name`, or the absence of one) onto the [`Role`] the admission gate
/// evaluates — **failing closed** for any caller pgmcp cannot positively identify
/// as a trusted white-box backbone.
///
/// The trust posture is *deny-by-default*:
///
///   * An **unidentified** caller — `None`, an empty string, the literal
///     `"unknown"` (the peer never completed `initialize`), or `"cli"` (the
///     `call_tool_cli` dispatch path, which carries no per-call transport
///     identity) — is mapped onto the **first canonical black-box role**
///     (`"claude"`). It is therefore refused on the latent REPL edge exactly as a
///     known black-box agent would be: an unidentified caller can NEVER be
///     admitted. (The set is non-empty by construction; the `unwrap_or` arm is a
///     defensive fallback to the same literal so the mapping is total.)
///   * A **positively identified** caller is used verbatim (already lowercased by
///     `extract_caller`). If it is a known black-box identity it lands in
///     [`black_box_roles`] and is refused; a white-box backbone role (absent from
///     that set) survives the medium arm.
///
/// This is the *input*-side fix for the former fail-open defect: the gate's logic
/// ([`repl_admitted`]) was always correct, but the MCP surface fed it a constant
/// white-box role for every caller. Threading the real identity through here makes
/// the structural refusal actually bite at the wire.
pub fn caller_role(identity: Option<&str>) -> Role {
    let trimmed = identity.map(str::trim).unwrap_or("");
    let unidentified = trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("unknown")
        || trimmed.eq_ignore_ascii_case("cli");
    if unidentified {
        // Fail closed: borrow a black-box role so the media-discipline arm refuses.
        return black_box_roles()
            .into_iter()
            .next()
            .unwrap_or_else(|| Role::new("claude"));
    }
    Role::new(trimmed.to_lowercase())
}

/// The outcome of the [`repl_admitted`] gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplAdmission {
    /// The REPL is admitted: a white-box caller AND an open experiment.
    Admitted,
    /// The REPL is refused, with a human-readable, structural reason. Logged at
    /// `warn!` by [`repl_admitted`] (ADR-021 trust-boundary-refused).
    Refused {
        /// Why admission was denied (medium / trust boundary, or experiment
        /// status). Safe to surface to the caller.
        reason: String,
    },
}

impl ReplAdmission {
    /// Whether the REPL was admitted.
    #[inline]
    pub fn is_admitted(&self) -> bool {
        matches!(self, ReplAdmission::Admitted)
    }

    /// The refusal reason, if refused.
    #[inline]
    pub fn refusal_reason(&self) -> Option<&str> {
        match self {
            ReplAdmission::Refused { reason } => Some(reason.as_str()),
            ReplAdmission::Admitted => None,
        }
    }
}

/// **The pure, synchronous admission gate** (future `TapeController::repl_admitted`).
///
/// Admits the REPL **iff both** the white-box-medium arm and the open-experiment
/// arm pass; otherwise returns [`ReplAdmission::Refused`] and logs the refusal at
/// `warn!` (ADR-021 trust-boundary-refused).
///
/// # Arguments
///
/// * `caller` — the calling role. In pgmcp's structural-trust model this is a
///   *host-supplied* identity, never a value an agent self-certifies on the wire.
/// * `black_box` — the trustworthy structural registry of black-box roles (agents
///   with no hidden-state access). A caller present in this set is refused on the
///   `Latent` REPL edge.
/// * `experiment_status` — the **already-resolved** status of the named experiment
///   (resolved from Postgres by the async caller). `None` means the slug did not
///   resolve to an experiment.
///
/// This function performs **no I/O** — it is a deterministic function of its
/// inputs, so it is fully unit-testable without a database.
pub fn repl_admitted(
    caller: &Role,
    black_box: &BTreeSet<Role>,
    experiment_status: Option<ExperimentStatus>,
) -> ReplAdmission {
    // --- Arm 1: white-box medium (structural, via the media discipline). ---
    // Model the REPL as the one-edge global type `O → caller : repl_dsl (Latent)`
    // and run the SAME projection-time discipline the CSM projector uses. A
    // black-box caller on this latent edge is rejected — "you cannot put Claude
    // in the latent loop."
    let edge = interaction(REPL_OFFERER, caller.clone(), repl_edge_label(), end());
    if let Err(media_err) = check_media_discipline(&edge, black_box) {
        let reason = format!(
            "REPL refused: {} (the tape REPL is the white-box / latent tier; a \
             black-box caller cannot be admitted, and white-box status is a \
             host-side structural fact, not a self-reported claim)",
            media_err.message()
        );
        tracing::warn!(
            caller = %caller,
            "tape_repl admission refused (trust boundary): {reason}"
        );
        return ReplAdmission::Refused { reason };
    }

    // --- Arm 2: open experiment (DB-backed, resolved upstream). ---
    match experiment_status {
        Some(ExperimentStatus::Open) => ReplAdmission::Admitted,
        other => {
            let reason = match other {
                None => "REPL refused: the named experiment does not exist (admission \
                         requires an Open experiment)"
                    .to_string(),
                Some(status) => format!(
                    "REPL refused: experiment status is '{}', but admission requires '{}'",
                    status.as_str(),
                    ExperimentStatus::Open.as_str()
                ),
            };
            tracing::warn!(
                caller = %caller,
                "tape_repl admission refused (experiment not open): {reason}"
            );
            ReplAdmission::Refused { reason }
        }
    }
}

/// The result of an **admitted** REPL run, shaped for JSON serialization by the
/// tool body. A budget exhaustion is reported as `over_limit: true` with a
/// structured `limit` label (NOT a 500 / transport error); a genuine
/// parse/eval/verb fault surfaces as `error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplRunResult {
    /// The rhai string rendering of the script's result value (empty for a
    /// unit-valued script). Faithful to the engine's typed `Dynamic` without
    /// pulling `rhai` into pgmcp's dependency graph.
    pub value: String,
    /// The rhai type name of the result value (e.g. `"string"`, `"array"`,
    /// `"map"`, `"i64"`, `"()"`), for callers that want to discriminate.
    pub value_type: String,
    /// Pages touched by the run (the host paging-pressure budget consumed).
    pub pages_touched: u64,
    /// Bytes touched by the run (the host working-set budget consumed).
    pub bytes_touched: u64,
    /// rhai operations the run executed under (the configured ceiling — rhai
    /// does not expose a live success-path tally).
    pub ops: u64,
    /// `true` iff a resource budget (rhai or host) was exhausted and the run was
    /// aborted; the abort is a *structured* outcome, not an error.
    pub over_limit: bool,
    /// When `over_limit`, the human label of the budget that tripped
    /// (`"operations"` / `"pages-touched"` / `"bytes-touched"` / `"call-levels"`
    /// / `"string-size"`); otherwise `None`.
    pub limit: Option<String>,
    /// A non-budget execution fault (parse / eval / verb-argument), if any. A
    /// faulting script is reported here rather than as a transport error so the
    /// caller can inspect it.
    pub error: Option<String>,
}

impl ReplRunResult {
    /// An empty (unit) successful result with the given budget tallies.
    fn unit(pages_touched: u64, bytes_touched: u64, ops: u64) -> Self {
        Self {
            value: String::new(),
            value_type: "()".to_string(),
            pages_touched,
            bytes_touched,
            ops,
            over_limit: false,
            limit: None,
            error: None,
        }
    }
}

/// Run a sandboxed REPL `script` against the per-tree [`context_tape::TapeStore`]
/// resolved from `ctx`'s [`TapeRegistry`](crate::tape::registry::TapeRegistry).
///
/// **Admission is the caller's responsibility** — by the time this runs the gate
/// ([`repl_admitted`]) has already admitted the call. This function only *lends
/// the store to the synchronous engine* and maps the typed
/// [`ReplOutcome`](context_tape::repl::ReplOutcome) / [`ReplError`] onto a
/// JSON-shaped [`ReplRunResult`].
///
/// ## Why no `&mut TapeStore` crosses an `await`
///
/// The store is borrowed **only inside the synchronous
/// [`with_store_mut`](crate::tape::registry::TapeRegistry::with_store_mut)
/// closure**, and [`ReplEngine::run`] is itself synchronous (it spawns no thread
/// and awaits nothing). The closure's signature is `FnOnce(&mut TapeStore) -> R`,
/// so the borrow *cannot* outlive the closure and there is no `.await` point
/// within it — nothing un-`Send` ever crosses an await boundary. The engine is
/// constructed inside the closure too, so neither the engine nor the borrow
/// escapes. This is the same lending discipline the context-tape engine documents
/// for its own `run`.
///
/// `tree` is the recursion-tree scope (its [`TreeId`] selects the per-tree store);
/// `limits` are the (already-mapped) [`ReplLimits`]. Exclusive (`with_store_mut`)
/// locking is required because the REPL's `put` verb mutates Scratch pages — the
/// corpus is never written (context-tape's store has no corpus-write path).
pub fn run_repl(
    ctx: &SystemContext,
    tree_id: TreeId,
    script: &str,
    limits: ReplLimits,
) -> ReplRunResult {
    ctx.tape_registry().with_store_mut(tree_id, |store| {
        // Construct and run the synchronous engine entirely within this closure.
        // No `.await` is reachable here, so the `&mut TapeStore` borrow never
        // crosses an await point.
        let mut engine = ReplEngine::new(limits);
        match engine.run(store, script) {
            Ok(outcome) => {
                let value = outcome.value.to_string();
                let value_type = outcome.value.type_name().to_string();
                // A rhai unit value renders as the empty string; normalise its
                // reported value to empty + "()" for a clean structured result.
                if outcome.value.is_unit() {
                    return ReplRunResult::unit(
                        outcome.pages_touched,
                        outcome.bytes_touched,
                        outcome.ops,
                    );
                }
                ReplRunResult {
                    value,
                    value_type,
                    pages_touched: outcome.pages_touched,
                    bytes_touched: outcome.bytes_touched,
                    ops: outcome.ops,
                    over_limit: false,
                    limit: None,
                    error: None,
                }
            }
            // A budget abort is a STRUCTURED outcome (`over_limit: true`), never
            // a transport error.
            Err(ReplError::LimitExceeded(kind)) => ReplRunResult {
                value: String::new(),
                value_type: "()".to_string(),
                pages_touched: 0,
                bytes_touched: 0,
                ops: limits.max_operations,
                over_limit: true,
                limit: Some(kind.label().to_string()),
                error: None,
            },
            // A non-budget fault (parse / eval / verb) is reported in `error`.
            Err(other) => ReplRunResult {
                value: String::new(),
                value_type: "()".to_string(),
                pages_touched: 0,
                bytes_touched: 0,
                ops: limits.max_operations,
                over_limit: false,
                limit: None,
                error: Some(other.to_string()),
            },
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csm::role::Role;

    /// Build a black-box role set from name literals (mirrors the csm media-test
    /// `bb(...)` helper so the trust model reads identically across modules).
    fn bb(roles: &[&str]) -> BTreeSet<Role> {
        roles.iter().map(|r| Role::new(*r)).collect()
    }

    #[test]
    fn black_box_caller_refused() {
        // A black-box caller (Claude) on the latent REPL edge is refused even with
        // an Open experiment — the medium-discipline arm fails first.
        let admission = repl_admitted(
            &Role::new("Claude"),
            &bb(&["Claude", "Codex"]),
            Some(ExperimentStatus::Open),
        );
        let reason = admission
            .refusal_reason()
            .expect("black-box caller must be refused");
        assert!(
            reason.contains("latent") || reason.contains("black-box"),
            "refusal must cite the medium / trust boundary; got: {reason}"
        );
    }

    #[test]
    fn white_box_plus_open_experiment_admitted() {
        // A white-box backbone role (absent from the black-box set) over the latent
        // edge AND an Open experiment ⇒ admitted.
        let admission = repl_admitted(
            &Role::new("Reflector"),
            &bb(&["Claude", "Codex"]),
            Some(ExperimentStatus::Open),
        );
        assert_eq!(
            admission,
            ReplAdmission::Admitted,
            "white-box caller + open experiment must be admitted"
        );
    }

    #[test]
    fn closed_experiment_refused() {
        // White-box on the medium arm, but the experiment is not Open ⇒ refused on
        // the experiment arm (status ≠ Open).
        for status in [
            ExperimentStatus::Measuring,
            ExperimentStatus::Decided,
            ExperimentStatus::Abandoned,
            ExperimentStatus::Superseded,
        ] {
            let admission = repl_admitted(&Role::new("Reflector"), &bb(&["Claude"]), Some(status));
            let reason = admission
                .refusal_reason()
                .unwrap_or_else(|| panic!("status {} must be refused", status.as_str()));
            assert!(
                reason.contains("status") && reason.contains("open"),
                "refusal for {} must cite the experiment status; got: {reason}",
                status.as_str()
            );
        }
        // A non-existent experiment (None) is likewise refused.
        let admission = repl_admitted(&Role::new("Reflector"), &bb(&["Claude"]), None);
        assert!(
            admission
                .refusal_reason()
                .expect("missing experiment refused")
                .contains("does not exist"),
            "a missing experiment must be refused with a 'does not exist' reason"
        );
    }

    #[test]
    fn latent_edge_with_black_box_role_refused() {
        // Tie to the existing csm media test (`black_box_role_on_latent_edge_is_rejected`):
        // the SAME `check_media_discipline` that rejects a black-box role on a latent
        // edge is what the gate consults, so a black-box caller is structurally
        // refused regardless of any (absent) self-reported medium. Demonstrate the
        // discipline directly, then through the gate, so the two agree.
        let caller = Role::new("Codex");
        let black_box = bb(&["Codex"]);

        // Directly: the one-edge latent protocol fails the discipline.
        let edge = interaction(REPL_OFFERER, caller.clone(), repl_edge_label(), end());
        assert!(
            check_media_discipline(&edge, &black_box).is_err(),
            "a Latent edge touching a black-box role must violate the discipline"
        );

        // Through the gate: refused on the medium arm even with an Open experiment.
        let admission = repl_admitted(&caller, &black_box, Some(ExperimentStatus::Open));
        assert!(
            !admission.is_admitted(),
            "the gate must refuse a black-box caller on the latent REPL edge"
        );
    }

    #[test]
    fn white_box_role_on_latent_edge_admitted_parity_with_media_test() {
        // Parity with `latent_edge_between_white_box_roles_passes`: neither the
        // offerer nor a white-box caller is black-box, so the latent edge is legal
        // and (with an Open experiment) the gate admits.
        let caller = Role::new("Reflector");
        let black_box = bb(&["Claude", "Codex"]);
        let edge = interaction(REPL_OFFERER, caller.clone(), repl_edge_label(), end());
        assert!(
            check_media_discipline(&edge, &black_box).is_ok(),
            "a latent edge between white-box roles is legal"
        );
        assert!(repl_admitted(&caller, &black_box, Some(ExperimentStatus::Open)).is_admitted());
    }

    // -- The casing fix: the canonical black-box set matches `extract_caller`. --

    #[test]
    fn canonical_black_box_roles_are_lowercase() {
        // `extract_caller` lowercases `clientInfo.name`, so the canonical registry
        // MUST be lowercase or the comparison silently fails open. This is the
        // exact regression the fix closes (the old set used TitleCase
        // "Claude"/"Codex", which never matched the lowercased wire identity).
        let set = black_box_roles();
        assert!(
            set.contains(&Role::new("claude")) && set.contains(&Role::new("codex")),
            "black_box_roles() must contain the lowercased canonical agent identities"
        );
        for r in &set {
            assert_eq!(
                r.as_str(),
                r.as_str().to_lowercase(),
                "every canonical black-box role must be lowercase; found {r}"
            );
        }
    }

    // -- The fail-closed mapping: unidentified callers become black-box. --

    #[test]
    fn lowercase_black_box_identity_maps_to_black_box_role_and_is_refused() {
        // The real-world defect: a wire caller "claude" (lowercase, as
        // `extract_caller` produces) must map to a role IN the canonical set and be
        // refused on the latent edge even with an Open experiment.
        let role = caller_role(Some("claude"));
        assert!(
            black_box_roles().contains(&role),
            "lowercase 'claude' must map onto a canonical black-box role"
        );
        let admission = repl_admitted(&role, &black_box_roles(), Some(ExperimentStatus::Open));
        let reason = admission
            .refusal_reason()
            .expect("a black-box caller must be refused through the canonical set");
        assert!(
            reason.contains("latent") || reason.contains("black-box"),
            "refusal must cite the medium / trust boundary; got: {reason}"
        );
    }

    #[test]
    fn unidentified_caller_fails_closed() {
        // Every unidentified shape — None, "", "unknown", "cli", and casing
        // variants — must map onto a black-box role so admission fails closed.
        for ident in [
            None,
            Some(""),
            Some("   "),
            Some("unknown"),
            Some("UNKNOWN"),
            Some("cli"),
            Some("CLI"),
        ] {
            let role = caller_role(ident);
            assert!(
                black_box_roles().contains(&role),
                "unidentified caller {ident:?} must fail closed onto a black-box role; got {role}"
            );
            assert!(
                !repl_admitted(&role, &black_box_roles(), Some(ExperimentStatus::Open))
                    .is_admitted(),
                "unidentified caller {ident:?} must be REFUSED even with an Open experiment"
            );
        }
    }

    #[test]
    fn identified_white_box_caller_is_admitted() {
        // A positively-identified white-box backbone role (absent from the canonical
        // set) is used verbatim and — with an Open experiment — admitted.
        let role = caller_role(Some("reflector"));
        assert!(
            !black_box_roles().contains(&role),
            "a white-box backbone identity must NOT be in the black-box set"
        );
        assert_eq!(
            role.as_str(),
            "reflector",
            "identity is used verbatim (lowercased)"
        );
        assert!(
            repl_admitted(&role, &black_box_roles(), Some(ExperimentStatus::Open)).is_admitted(),
            "an identified white-box caller + open experiment must be admitted"
        );
    }

    #[test]
    fn identified_caller_is_lowercased() {
        // A mixed-case identity is normalised to lowercase so it compares equal to
        // the canonical (lowercase) black-box set — defence in depth against a
        // client that announces a TitleCase clientInfo.name.
        let role = caller_role(Some("Claude"));
        assert!(
            black_box_roles().contains(&role),
            "a TitleCase 'Claude' must still normalise into the black-box set"
        );
    }
}
