------------------------------ MODULE SecurityAuditBoundary ------------------------------
(***************************************************************************)
(* Direct MCP boundaries for the remaining Phase 6 security audit tools:   *)
(* `taint_analysis`, `injection_candidates`, `crypto_misuse`, and           *)
(* `unsafe_deserialization`.                                                *)
(*                                                                         *)
(* The implementation resolves one normalized project id, clamps output     *)
(* limits before scanning, streams indexed file content, applies each        *)
(* security predicate before the cap, drops content streams before effect    *)
(* enrichment, bounds enrichment rows, and performs no writes or locks.      *)
(***************************************************************************)

EXTENDS Naturals

Tools == {"taint", "injection", "crypto", "deserialize"}
DataflowTools == {"taint", "injection"}
AstTools == {"crypto", "deserialize"}
HeuristicTools == Tools
PredicateTools == {"injection", "crypto", "deserialize"}

MaxLimit == 500
MaxEffectSymbols == 500

Reasons ==
    {"none", "blank_project", "missing_project", "duplicate_project", "bad_kind"}

NoReq ==
    [ id |-> 0,
      tool |-> "",
      project |-> "",
      kind |-> "",
      limit |-> 50,
      row_scope |-> "same",
      dataflow |-> 0,
      interproc |-> 0,
      ast |-> 0,
      heuristic |-> 0,
      effect |-> 0 ]

Requests ==
    { [ id |-> 1, tool |-> "taint", project |-> " p6-sec ", kind |-> "",
        limit |-> 0, row_scope |-> "same", dataflow |-> 2, interproc |-> 2,
        ast |-> 0, heuristic |-> 2, effect |-> 2 ],
      [ id |-> 2, tool |-> "injection", project |-> "p6-sec", kind |-> " sql ",
        limit |-> 1, row_scope |-> "same", dataflow |-> 2, interproc |-> 1,
        ast |-> 0, heuristic |-> 2, effect |-> 2 ],
      [ id |-> 3, tool |-> "injection", project |-> "p6-sec", kind |-> "sql-ish",
        limit |-> 50, row_scope |-> "same", dataflow |-> 1, interproc |-> 1,
        ast |-> 0, heuristic |-> 1, effect |-> 1 ],
      [ id |-> 4, tool |-> "crypto", project |-> "p6-sec", kind |-> "",
        limit |-> 999, row_scope |-> "same", dataflow |-> 0, interproc |-> 0,
        ast |-> 600, heuristic |-> 600, effect |-> 600 ],
      [ id |-> 5, tool |-> "deserialize", project |-> " p6-sec ", kind |-> "",
        limit |-> 0, row_scope |-> "same", dataflow |-> 0, interproc |-> 0,
        ast |-> 1, heuristic |-> 1, effect |-> 1 ],
      [ id |-> 6, tool |-> "taint", project |-> "   ", kind |-> "",
        limit |-> 50, row_scope |-> "same", dataflow |-> 1, interproc |-> 1,
        ast |-> 0, heuristic |-> 1, effect |-> 1 ],
      [ id |-> 7, tool |-> "crypto", project |-> "missing", kind |-> "",
        limit |-> 50, row_scope |-> "same", dataflow |-> 0, interproc |-> 0,
        ast |-> 1, heuristic |-> 1, effect |-> 1 ],
      [ id |-> 8, tool |-> "deserialize", project |-> "dupe", kind |-> "",
        limit |-> 50, row_scope |-> "same", dataflow |-> 0, interproc |-> 0,
        ast |-> 1, heuristic |-> 1, effect |-> 1 ],
      [ id |-> 9, tool |-> "injection", project |-> "p6-sec", kind |-> "all",
        limit |-> 50, row_scope |-> "cross", dataflow |-> 3, interproc |-> 3,
        ast |-> 0, heuristic |-> 3, effect |-> 3 ] }

TrimProject(p) ==
    CASE p = " p6-sec " -> "p6-sec"
      [] p = "   " -> ""
      [] OTHER -> p

TrimKind(k) ==
    CASE k = " sql " -> "sql"
      [] k = "" -> "all"
      [] OTHER -> k

ProjectState(p) ==
    CASE TrimProject(p) = "" -> "blank_project"
      [] TrimProject(p) = "missing" -> "missing_project"
      [] TrimProject(p) = "dupe" -> "duplicate_project"
      [] OTHER -> "ok"

EffectiveKind(r) ==
    IF r.tool = "injection" THEN TrimKind(r.kind) ELSE "none"

KindState(r) ==
    IF r.tool # "injection" THEN "ok"
    ELSE IF EffectiveKind(r) \in {"all", "sql", "shell"} THEN "ok"
    ELSE "bad_kind"

ClampLimit(n) ==
    IF n < 1 THEN 1 ELSE IF n > MaxLimit THEN MaxLimit ELSE n

Min(a, b) == IF a < b THEN a ELSE b

Scoped(n, row_scope) ==
    IF row_scope = "same" THEN n ELSE 0

HasRows(r, n) ==
    Scoped(n, r.row_scope) > 0

Reject(reason, lookups) ==
    [ rejected |-> TRUE,
      reason |-> reason,
      normalized_project |-> "",
      effective_kind |-> "none",
      effective_limit |-> 50,
      effect_symbol_limit |-> MaxEffectSymbols,
      project_lookups |-> lookups,
      dataflow_stream_resident |-> 0,
      ast_stream_resident |-> 0,
      heuristic_stream_resident |-> 0,
      stream_open_at_enrichment |-> FALSE,
      enrichment_runs |-> FALSE,
      dataflow_findings |-> 0,
      interproc_findings |-> 0,
      ast_findings |-> 0,
      heuristic_findings |-> 0,
      effect_symbols |-> 0,
      rows_project_scoped |-> TRUE,
      predicate_before_cap |-> TRUE,
      writes |-> 0,
      locks_held |-> 0 ]

Accept(r) ==
    LET lim == ClampLimit(r.limit) IN
    [ rejected |-> FALSE,
      reason |-> "none",
      normalized_project |-> TrimProject(r.project),
      effective_kind |-> EffectiveKind(r),
      effective_limit |-> lim,
      effect_symbol_limit |-> MaxEffectSymbols,
      project_lookups |-> 1,
      dataflow_stream_resident |->
          IF r.tool \in DataflowTools /\ HasRows(r, r.dataflow + r.interproc) THEN 1 ELSE 0,
      ast_stream_resident |->
          IF r.tool \in AstTools /\ HasRows(r, r.ast) THEN 1 ELSE 0,
      heuristic_stream_resident |->
          IF r.tool \in HeuristicTools /\ HasRows(r, r.heuristic) THEN 1 ELSE 0,
      stream_open_at_enrichment |-> FALSE,
      enrichment_runs |-> TRUE,
      dataflow_findings |->
          IF r.tool \in DataflowTools THEN Min(Scoped(r.dataflow, r.row_scope), lim) ELSE 0,
      interproc_findings |->
          IF r.tool \in DataflowTools THEN Min(Scoped(r.interproc, r.row_scope), lim) ELSE 0,
      ast_findings |->
          IF r.tool \in AstTools THEN Min(Scoped(r.ast, r.row_scope), lim) ELSE 0,
      heuristic_findings |-> Min(Scoped(r.heuristic, r.row_scope), lim),
      effect_symbols |-> Min(Scoped(r.effect, r.row_scope), MaxEffectSymbols),
      rows_project_scoped |-> TRUE,
      predicate_before_cap |-> TRUE,
      writes |-> 0,
      locks_held |-> 0 ]

Evaluate(r) ==
    IF ProjectState(r.project) # "ok" THEN Reject(ProjectState(r.project), 1)
    ELSE IF KindState(r) # "ok" THEN Reject(KindState(r), 1)
    ELSE Accept(r)

NoResp ==
    [ rejected |-> FALSE,
      reason |-> "none",
      normalized_project |-> "",
      effective_kind |-> "none",
      effective_limit |-> 50,
      effect_symbol_limit |-> MaxEffectSymbols,
      project_lookups |-> 0,
      dataflow_stream_resident |-> 0,
      ast_stream_resident |-> 0,
      heuristic_stream_resident |-> 0,
      stream_open_at_enrichment |-> FALSE,
      enrichment_runs |-> FALSE,
      dataflow_findings |-> 0,
      interproc_findings |-> 0,
      ast_findings |-> 0,
      heuristic_findings |-> 0,
      effect_symbols |-> 0,
      rows_project_scoped |-> TRUE,
      predicate_before_cap |-> TRUE,
      writes |-> 0,
      locks_held |-> 0 ]

VARIABLES req, resp

vars == <<req, resp>>

Init ==
    /\ req = NoReq
    /\ resp = NoResp

Handle(r) ==
    /\ req = NoReq
    /\ r \in Requests
    /\ req' = r
    /\ resp' = Evaluate(r)

Done ==
    /\ req # NoReq
    /\ UNCHANGED vars

Next ==
    \/ \E r \in Requests : Handle(r)
    \/ Done

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ resp.rejected \in BOOLEAN
    /\ resp.reason \in Reasons
    /\ resp.normalized_project \in {"", "p6-sec"}
    /\ resp.effective_kind \in {"none", "all", "sql", "shell"}
    /\ resp.effective_limit \in 1..MaxLimit
    /\ resp.effect_symbol_limit = MaxEffectSymbols
    /\ resp.project_lookups \in 0..1
    /\ resp.dataflow_stream_resident \in 0..1
    /\ resp.ast_stream_resident \in 0..1
    /\ resp.heuristic_stream_resident \in 0..1
    /\ resp.stream_open_at_enrichment \in BOOLEAN
    /\ resp.enrichment_runs \in BOOLEAN
    /\ resp.dataflow_findings \in 0..MaxLimit
    /\ resp.interproc_findings \in 0..MaxLimit
    /\ resp.ast_findings \in 0..MaxLimit
    /\ resp.heuristic_findings \in 0..MaxLimit
    /\ resp.effect_symbols \in 0..MaxEffectSymbols
    /\ resp.rows_project_scoped \in BOOLEAN
    /\ resp.predicate_before_cap \in BOOLEAN
    /\ resp.writes = 0
    /\ resp.locks_held = 0

InvalidInputsDoNotScan ==
    req # NoReq /\ resp.rejected =>
        /\ resp.dataflow_stream_resident = 0
        /\ resp.ast_stream_resident = 0
        /\ resp.heuristic_stream_resident = 0
        /\ resp.enrichment_runs = FALSE
        /\ resp.dataflow_findings = 0
        /\ resp.interproc_findings = 0
        /\ resp.ast_findings = 0
        /\ resp.heuristic_findings = 0
        /\ resp.effect_symbols = 0

ProjectRejectsBeforeScan ==
    req # NoReq /\ resp.reason \in {"blank_project", "missing_project", "duplicate_project"} =>
        /\ resp.project_lookups = 1
        /\ resp.enrichment_runs = FALSE

BadKindRejectsBeforeScan ==
    req # NoReq /\ resp.reason = "bad_kind" =>
        /\ req.tool = "injection"
        /\ resp.project_lookups = 1
        /\ resp.enrichment_runs = FALSE

EffectiveBoundsHold ==
    req # NoReq /\ ~resp.rejected =>
        /\ resp.effective_limit <= MaxLimit
        /\ resp.dataflow_findings <= resp.effective_limit
        /\ resp.interproc_findings <= resp.effective_limit
        /\ resp.ast_findings <= resp.effective_limit
        /\ resp.heuristic_findings <= resp.effective_limit
        /\ resp.effect_symbols <= resp.effect_symbol_limit

StreamingMemoryBound ==
    req # NoReq =>
        /\ resp.dataflow_stream_resident <= 1
        /\ resp.ast_stream_resident <= 1
        /\ resp.heuristic_stream_resident <= 1

EnrichmentAfterStreamDrop ==
    req # NoReq /\ resp.enrichment_runs =>
        resp.stream_open_at_enrichment = FALSE

ScopedRowsOnly ==
    req # NoReq /\ ~resp.rejected /\ req.row_scope = "cross" =>
        /\ resp.dataflow_findings = 0
        /\ resp.interproc_findings = 0
        /\ resp.ast_findings = 0
        /\ resp.heuristic_findings = 0
        /\ resp.effect_symbols = 0

PredicateBeforeCap ==
    req # NoReq /\ ~resp.rejected /\ req.tool \in PredicateTools =>
        /\ resp.predicate_before_cap = TRUE
        /\ (req.tool = "injection" =>
            resp.dataflow_findings = Min(Scoped(req.dataflow, req.row_scope), resp.effective_limit))
        /\ (req.tool \in AstTools =>
            resp.ast_findings = Min(Scoped(req.ast, req.row_scope), resp.effective_limit))

ReadOnly ==
    req # NoReq => resp.writes = 0

NoRetainedLocks ==
    req # NoReq => resp.locks_held = 0

=============================================================================
