------------------------------ MODULE ApiStabilityScope ------------------------------
(***************************************************************************)
(* `api_stability` request/scoping model.                                  *)
(*                                                                         *)
(* The tool resolves one project id, clamps the recent-commit window and    *)
(* result limit, reads git_commit_chunks.content through commits scoped to  *)
(* that project, and reuses the resolved id for effect enrichment.          *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

MaxWindow == 10
MaxLimit == 5

ProjectModes == {"unique", "blank", "missing", "duplicate"}
Outcomes == {"ok", "rejected"}
Reasons == {"none", "invalid_project"}
ProjectIds == {0, 1, 2}
Columns == {"none", "content", "chunk_text"}
Symbols == {"changed_api", "leaked_api"}

CommitRows ==
    { [project_id |-> 1, symbol |-> "changed_api", signature |-> TRUE],
      [project_id |-> 2, symbol |-> "leaked_api", signature |-> TRUE],
      [project_id |-> 1, symbol |-> "changed_api", signature |-> FALSE] }

Requests ==
    { [id |-> 1, project_mode |-> "unique", raw_window |-> 0,
       raw_limit |-> -10, available |-> 1],
      [id |-> 2, project_mode |-> "unique", raw_window |-> 999,
       raw_limit |-> 999, available |-> 3],
      [id |-> 3, project_mode |-> "blank", raw_window |-> 100,
       raw_limit |-> 50, available |-> 1],
      [id |-> 4, project_mode |-> "duplicate", raw_window |-> 100,
       raw_limit |-> 50, available |-> 1],
      [id |-> 5, project_mode |-> "missing", raw_window |-> 100,
       raw_limit |-> 50, available |-> 1] }

RequestIds == {r.id : r \in Requests}

Clamp(v, lo, hi) ==
    IF v < lo THEN lo ELSE IF v > hi THEN hi ELSE v

Min(a, b) == IF a <= b THEN a ELSE b

ProjectFor(r) ==
    IF r.project_mode = "unique" THEN 1 ELSE 0

ReasonFor(r) ==
    IF r.project_mode = "unique" THEN "none" ELSE "invalid_project"

ScopedRowsFor(r) ==
    IF ReasonFor(r) # "none" THEN {}
    ELSE {row \in CommitRows : row.project_id = ProjectFor(r) /\ row.signature}

ResponseFor(r) ==
    LET reason == ReasonFor(r) IN
    LET ok == reason = "none" IN
    LET window == Clamp(r.raw_window, 1, MaxWindow) IN
    LET limit == Clamp(r.raw_limit, 1, MaxLimit) IN
        [ request_id |-> r.id,
          outcome |-> IF ok THEN "ok" ELSE "rejected",
          reason |-> reason,
          project_id |-> IF ok THEN ProjectFor(r) ELSE 0,
          commit_column |-> IF ok THEN "content" ELSE "none",
          window |-> IF ok THEN window ELSE 0,
          limit |-> IF ok THEN limit ELSE 0,
          result_count |-> IF ok THEN Min(Cardinality(ScopedRowsFor(r)), limit) ELSE 0,
          result_projects |-> {row.project_id : row \in ScopedRowsFor(r)},
          result_symbols |-> {row.symbol : row \in ScopedRowsFor(r)},
          effect_project_id |-> IF ok THEN ProjectFor(r) ELSE 0,
          writes |-> 0,
          lock_held |-> FALSE ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      reason: Reasons,
      project_id: ProjectIds,
      commit_column: Columns,
      window: 0..MaxWindow,
      limit: 0..MaxLimit,
      result_count: 0..MaxLimit,
      result_projects: SUBSET ProjectIds,
      result_symbols: SUBSET Symbols,
      effect_project_id: ProjectIds,
      writes: 0..0,
      lock_held: BOOLEAN ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK == response \in ResponseRecord

InvalidProjectsReject ==
    ReasonFor(req) = "invalid_project" =>
        /\ response.outcome = "rejected"
        /\ response.result_count = 0

WindowAndLimitBounded ==
    response.outcome = "ok" =>
        /\ response.window \in 1..MaxWindow
        /\ response.limit \in 1..MaxLimit
        /\ response.result_count <= response.limit

UsesCurrentCommitChunkColumn ==
    response.outcome = "ok" => response.commit_column = "content"

CommitRowsStayProjectScoped ==
    response.outcome = "ok" => response.result_projects \subseteq {response.project_id}

EffectEnrichmentUsesResolvedProject ==
    response.outcome = "ok" => response.effect_project_id = response.project_id

NoCrossProjectLeak ==
    response.outcome = "ok" /\ response.project_id = 1 =>
        "leaked_api" \notin response.result_symbols

ReadOnlyNoHeldLock ==
    /\ response.writes = 0
    /\ response.lock_held = FALSE

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidProjectsReject /\
        WindowAndLimitBounded /\
        UsesCurrentCommitChunkColumn /\
        CommitRowsStayProjectScoped /\
        EffectEnrichmentUsesResolvedProject /\
        NoCrossProjectLeak /\
        ReadOnlyNoHeldLock)

================================================================================
