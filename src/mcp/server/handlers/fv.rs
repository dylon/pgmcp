//! Native formal-verification (FV) tool handlers (Task #22 §4-A).
//!
//! Each tool runs entirely in-process — pgmcp/CSM data + `lling_llang::symbolic`
//! engines → verdict — with **no subprocess and no prattail dependency**. The router
//! `router_fv` is composed into `assembled_tool_router()` in `server.rs`.

use crate::mcp::server::McpServer;
use crate::mcp::server::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};

#[rmcp::tool_router(router = router_fv, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Protocol soundness — deadlock-freedom + progress for a CSM \
            GlobalType, decided IN-PROCESS via MPST well-formedness. Rocq-backed by \
            CsmDeadlockFreedom.v (well_formed ⇒ deadlock-free ∧ progress by typing, \
            Caires–Pfenning/Wadler) — closes the ADR-012 gap that otherwise needs \
            pi+TLC. No subprocess, no prattail. Param: global_type (adjacent-tagged JSON)."
    )]
    async fn protocol_soundness(
        &self,
        Parameters(params): Parameters<ProtocolSoundnessParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "protocol_soundness",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_protocol_soundness::tool_protocol_soundness(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Language inclusion L(impl) ⊆ L(spec) over Symbolic Finite \
            Automata (lling-llang), via is_empty(impl ∩ ¬spec). The merge-coordinator \
            feature-preservation primitive: 'no exported behavior lost'. A non-empty \
            residual is a falsifiable witness word. In-process; no prattail. Params: \
            impl_sfa, spec_sfa (states/initial/accepting/transitions over [lo,hi) guards)."
    )]
    async fn language_inclusion(
        &self,
        Parameters(params): Parameters<LanguageInclusionParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "language_inclusion",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_language_inclusion::tool_language_inclusion(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Presburger-arithmetic satisfiability via the automata-based \
            decision procedure in lling-llang (PresburgerNfa). In-process; no \
            subprocess, no prattail. Params: formula (True/False/Atom{terms,rhs,rel}/ \
            And/Or/Not/Exists), bit_width (default 8)."
    )]
    async fn presburger_decide(
        &self,
        Parameters(params): Parameters<PresburgerDecideParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "presburger_decide",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_presburger_decide::tool_presburger_decide(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Effect-policy conformance: do the effects reachable from a seed \
            symbol (over the resolved-call subgraph) stay within an allowed set? Sound \
            inclusion reachable ⊆ allowed; each violation reports its shortest call \
            depth. In-process; no prattail. Params: seed_symbol_id, allowed_effects, \
            max_depth (default 8)."
    )]
    async fn effect_verify(
        &self,
        Parameters(params): Parameters<EffectVerifyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "effect_verify",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_effect_verify::tool_effect_verify(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Behavioral check — CTL model-checking of a finite labelled \
            transition system (Clarke–Emerson–Sistla fixpoint labelling). Full CTL \
            (EX/AX/EF/AF/EG/AG/EU/AU + boolean); the branching-time complement to the \
            SFA/SMT/Presburger tools. In-process; no prattail. Params: num_states, \
            initial, transitions, labels (atoms per state), formula."
    )]
    async fn behavioral_check(
        &self,
        Parameters(params): Parameters<BehavioralCheckParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "behavioral_check",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_behavioral_check::tool_behavioral_check(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "KAT Hoare check — decide {precond} program {postcond} \
            (Kleene Algebra with Tests: p·c·¬q ≡ 0) over a finite Boolean state space, \
            via the hoisted lling-llang BooleanTest/eval_test_public. Returns a \
            falsifiable counterexample state on failure. In-process; no prattail. \
            Params: atoms, precond, program (assume/assign/havoc), postcond."
    )]
    async fn kat_hoare_check(
        &self,
        Parameters(params): Parameters<KatHoareCheckParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "kat_hoare_check",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_kat_hoare_check::tool_kat_hoare_check(self.ctx(), params),
        )
        .await
    }
}
