//! CSM / MPST coordination observer tool handlers (ADR-009).
//!
//! Tool handlers extracted verbatim from `server.rs` (B.3 god-file split).
//! Only the relative `super::tools::` path was rewritten to the absolute
//! `crate::mcp::tools::` (the module is unchanged); bodies are otherwise
//! identical. The per-block router is composed in `server.rs` via
//! `assembled_tool_router()`.
#![allow(clippy::too_many_lines)]

use crate::mcp::server::McpServer;
use crate::mcp::server::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};

#[rmcp::tool_router(router = router_csm, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "List the CSM/MPST coordination protocols (the five RecursiveMAS patterns) \
with participants and well-formedness. ADR-009."
    )]
    async fn csm_list_protocols(
        &self,
        Parameters(params): Parameters<CsmListProtocolsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "csm_list_protocols",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_csm_list_protocols::tool_csm_list_protocols(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Show one coordination pattern's global type (the MPST AST), participants, \
and well-formedness. ADR-009."
    )]
    async fn csm_protocol_of_pattern(
        &self,
        Parameters(params): Parameters<CsmProtocolOfPatternParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "csm_protocol_of_pattern",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_csm_protocol_of_pattern::tool_csm_protocol_of_pattern(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Show the per-role local machines a coordination pattern projects to \
(G ↾ role); a role that does not project surfaces its projection error. ADR-009."
    )]
    async fn csm_show_projection(
        &self,
        Parameters(params): Parameters<CsmShowProjectionParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "csm_show_projection",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_csm_show_projection::tool_csm_show_projection(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Validate a completed a2a_pattern_* run against its coordination protocol: \
lift the recorded transcript into a trace, check conformance, and persist the verdict to \
csm_run_traces. ADR-009."
    )]
    async fn csm_validate_run(
        &self,
        Parameters(params): Parameters<CsmValidateRunParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "csm_validate_run",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_csm_validate_run::tool_csm_validate_run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Show the protocol interpreter's prescribed orchestrator communication order \
for a pattern (the ProtocolDriver plan). Linear patterns (sequential/mixture/distillation/recursive) \
are drivable; Deliberation is not (runtime choice). ADR-009 Phase 6."
    )]
    async fn csm_protocol_plan(
        &self,
        Parameters(params): Parameters<CsmProtocolPlanParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "csm_protocol_plan",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_csm_protocol_plan::tool_csm_protocol_plan(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Infer a peer's behaviour FSM from a protocol's accumulated run traces \
(passive prefix-tree automaton with observation counts) and diff it against the declared protocol — \
novel symbols flag off-protocol behaviour. ADR-009 Phase 8."
    )]
    async fn csm_infer_peer_fsm(
        &self,
        Parameters(params): Parameters<CsmInferPeerFsmParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "csm_infer_peer_fsm",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_csm_infer_peer_fsm::tool_csm_infer_peer_fsm(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Synthesize a typed Multiparty-Session-Type protocol from a work-item subtree \
(a plan): fold its actionable items into a GlobalType, optionally wrapped in a Critic-gated loop \
(cyclic Rec/Var), validate well-formedness + the black-box media discipline + per-role projection, \
and emit a client-drivable plan with role→peer bindings. The plan→state-machine→orchestrator \
keystone (Crucible E5). Read-only: synthesizes and validates; never executes work."
    )]
    async fn csm_synthesize_protocol(
        &self,
        Parameters(params): Parameters<CsmSynthesizeProtocolParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "csm_synthesize_protocol",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_csm_synthesize_protocol::tool_csm_synthesize_protocol(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Decompose a stored protocol's GlobalType into its MONOIDAL STRING DIAGRAM \
(ADR-028 CT-3): the sequential-composition spine (sequential_depth = interaction steps on the \
longest single trace) and the TENSOR factorization (⊗) — the partition of roles into independent \
parallel sub-protocols (union-find over every Interaction/Choice role pair). Roles in DIFFERENT \
tensor factors PROVABLY never communicate in this protocol, so they may be scheduled independently \
— the falsifiable, schedule-relevant payoff. Also reports recursion (Rec back-edges) and renders a \
unicode diagram. Loads from csm_protocols by public id or name. Read-only."
    )]
    async fn csm_protocol_string_diagram(
        &self,
        Parameters(params): Parameters<CsmProtocolStringDiagramParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "csm_protocol_string_diagram",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_csm_protocol_string_diagram::tool_csm_protocol_string_diagram(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Compute the FORMAL CONCEPT LATTICE (Formal Concept Analysis, ADR-028 CT-4) of \
a real formal context drawn from pgmcp's tables: objects × attributes × incidence. Two contexts: \
(object_kind=symbol, attribute_kind=effect) over file_symbols × effect_catalog via symbol_effects, \
or (object_kind=file, attribute_kind=type_tag) over indexed_files × type_tag_catalog via the \
has_type relation. Enumerates ALL formal concepts (extent, intent) via Ganter's NextClosure under \
the Galois connection A↦A'/B↦B', returns the Hasse covering lattice on extent inclusion and the \
extent-drop attribute implications (premise⟹conclusion). This lattice is COMPUTED from real \
incidence — distinct from the declared ontology is_a cover. max_concepts bounds enumeration (a \
truncation is logged and flagged). Read-only."
    )]
    async fn fca_concept_lattice(
        &self,
        Parameters(params): Parameters<FcaConceptLatticeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "fca_concept_lattice",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_fca_concept_lattice::tool_fca_concept_lattice(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Checkpoint a Crucible orchestration session (PAUSE/RESUME, ADR-009). UPSERTs \
the agent-provided checkpoint (protocol GlobalType, cursor, critic iteration, role→peer map, \
transcript) into pgmcp's own orchestration_sessions table by session_key. With pause=true it \
SUSPENDS the session: it GUARDS that every child a2a_task is terminal (refusing with \
{paused:false, reason} otherwise), flushes the transcript to csm_run_traces, and drops the \
work-item lease. PERSIST-only: pgmcp never runs a shell or writes the user's files."
    )]
    async fn session_checkpoint_save(
        &self,
        Parameters(params): Parameters<SessionCheckpointSaveParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "session_checkpoint_save",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_session_checkpoint_save::tool_session_checkpoint_save(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Resume a paused Crucible orchestration session (PAUSE/RESUME, ADR-009). Loads \
the checkpoint, rebuilds the projected network from the stored GlobalType, REPLAYS the recorded \
trace (csm_run_traces.events + the unflushed transcript) to recover the protocol position, and \
returns the orchestrator's next_step (peer/request/response) — or a critic_verdict await when it \
faces the Critic Choice. A corrupt trace is refused loudly. fork=true resumes a fresh child copy. \
REPLAY/VALIDATE-only: pgmcp re-claims the work-item lease and returns the plan; the orchestrator \
executes it. pgmcp never runs a shell or writes the user's files."
    )]
    async fn session_checkpoint_resume(
        &self,
        Parameters(params): Parameters<SessionCheckpointResumeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "session_checkpoint_resume",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_session_checkpoint_resume::tool_session_checkpoint_resume(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "List paused/resumable Crucible orchestration sessions (PAUSE/RESUME, ADR-009). \
Returns the suspended sessions newest-first with their cursor/critic position and work-item root, \
so an orchestrator can pick one to resume. READ-only over pgmcp's own orchestration_sessions \
table; pgmcp never runs a shell or writes the user's files."
    )]
    async fn session_checkpoint_list(
        &self,
        Parameters(params): Parameters<SessionCheckpointListParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "session_checkpoint_list",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_session_checkpoint_list::tool_session_checkpoint_list(
                self.ctx(),
                params,
            ),
        )
        .await
    }
}
