--------------------------- MODULE DeadCodeReachabilityScope ---------------------------
(***************************************************************************)
(* `dead_code_reachability` request/scoping model.                         *)
(*                                                                         *)
(* The tool resolves one project id, normalizes output bounds, chooses      *)
(* roots from public/entry symbols, walks only accepted same-project call   *)
(* edges, and reports unreached in-project candidates without writes or     *)
(* persistent locks.                                                       *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

ProjectIds == {0, 1, 2}
ProjectModes == {"unique", "blank", "missing", "duplicate"}
LimitModes == {"default", "huge", "negative"}
Outcomes == {"ok", "rejected", "soft_fail"}
Reasons == {"none", "invalid_project", "no_symbols"}
Names == {"main", "reachable", "via_bare", "dead", "test_entry", "test_only", "foreign"}

Symbols ==
    { [name |-> "main", project |-> 1, is_test |-> FALSE,
       is_root |-> TRUE, is_candidate |-> TRUE],
      [name |-> "reachable", project |-> 1, is_test |-> FALSE,
       is_root |-> FALSE, is_candidate |-> TRUE],
      [name |-> "via_bare", project |-> 1, is_test |-> FALSE,
       is_root |-> FALSE, is_candidate |-> TRUE],
      [name |-> "dead", project |-> 1, is_test |-> FALSE,
       is_root |-> FALSE, is_candidate |-> TRUE],
      [name |-> "test_entry", project |-> 1, is_test |-> TRUE,
       is_root |-> TRUE, is_candidate |-> TRUE],
      [name |-> "test_only", project |-> 1, is_test |-> FALSE,
       is_root |-> FALSE, is_candidate |-> TRUE],
      [name |-> "foreign", project |-> 2, is_test |-> FALSE,
       is_root |-> TRUE, is_candidate |-> TRUE] }

Edges ==
    { [source |-> "main", target |-> "reachable",
       source_project |-> 1, source_file_project |-> 1,
       target_project |-> 1, target_file_project |-> 1,
       kind |-> "exact_in_file"],
      [source |-> "main", target |-> "via_bare",
       source_project |-> 1, source_file_project |-> 1,
       target_project |-> 1, target_file_project |-> 1,
       kind |-> "bare_name_in_project"],
      [source |-> "test_entry", target |-> "test_only",
       source_project |-> 1, source_file_project |-> 1,
       target_project |-> 1, target_file_project |-> 1,
       kind |-> "exact_in_file"],
      \* Stale target-symbol row: source file belongs to project 1, target
      \* symbol belongs to project 2.
      [source |-> "main", target |-> "foreign",
       source_project |-> 1, source_file_project |-> 1,
       target_project |-> 2, target_file_project |-> 2,
       kind |-> "exact_in_file"],
      \* Stale source-symbol row: source_file_id belongs to project 1, but the
      \* source_symbol_id belongs to project 2.
      [source |-> "foreign", target |-> "dead",
       source_project |-> 2, source_file_project |-> 1,
       target_project |-> 1, target_file_project |-> 1,
       kind |-> "exact_in_file"] }

Requests ==
    { [id |-> 1, project_mode |-> "unique", include_tests |-> FALSE,
       include_bare |-> FALSE, limit_mode |-> "huge", symbols_present |-> TRUE],
      [id |-> 2, project_mode |-> "unique", include_tests |-> TRUE,
       include_bare |-> TRUE, limit_mode |-> "default", symbols_present |-> TRUE],
      [id |-> 3, project_mode |-> "unique", include_tests |-> FALSE,
       include_bare |-> TRUE, limit_mode |-> "negative", symbols_present |-> TRUE],
      [id |-> 4, project_mode |-> "blank", include_tests |-> FALSE,
       include_bare |-> FALSE, limit_mode |-> "default", symbols_present |-> TRUE],
      [id |-> 5, project_mode |-> "duplicate", include_tests |-> FALSE,
       include_bare |-> FALSE, limit_mode |-> "default", symbols_present |-> TRUE],
      [id |-> 6, project_mode |-> "unique", include_tests |-> FALSE,
       include_bare |-> FALSE, limit_mode |-> "default", symbols_present |-> FALSE] }

RequestIds == {r.id : r \in Requests}

ProjectFor(r) ==
    IF r.project_mode = "unique" THEN 1 ELSE 0

LimitFor(r) ==
    CASE r.limit_mode = "huge" -> 1000
      [] r.limit_mode = "negative" -> 1
      [] OTHER -> 50

ReasonFor(r) ==
    CASE r.project_mode # "unique" -> "invalid_project"
      [] ~r.symbols_present -> "no_symbols"
      [] OTHER -> "none"

RootSymbolsFor(r) ==
    IF ReasonFor(r) # "none" THEN {}
    ELSE {s \in Symbols :
            /\ s.project = ProjectFor(r)
            /\ s.is_root
            /\ (r.include_tests \/ ~s.is_test)}

RootNamesFor(r) ==
    {s.name : s \in RootSymbolsFor(r)}

CandidateSymbolsFor(r) ==
    IF ReasonFor(r) # "none" THEN {}
    ELSE {s \in Symbols :
            /\ s.project = ProjectFor(r)
            /\ s.is_candidate
            /\ (r.include_tests \/ ~s.is_test)}

CandidateNamesFor(r) ==
    {s.name : s \in CandidateSymbolsFor(r)}

EdgeAccepted(r, e) ==
    /\ ReasonFor(r) = "none"
    /\ e.source_project = ProjectFor(r)
    /\ e.source_file_project = ProjectFor(r)
    /\ e.target_project = ProjectFor(r)
    /\ e.target_file_project = ProjectFor(r)
    /\ (e.kind \in {"exact_in_file", "exact_via_import"} \/
        (r.include_bare /\ e.kind = "bare_name_in_project"))

AcceptedEdgesFor(r) ==
    {e \in Edges : EdgeAccepted(r, e)}

Step(r, reached) ==
    reached \cup {e.target : e \in {x \in AcceptedEdgesFor(r) : x.source \in reached}}

ReachFor(r) ==
    LET r0 == RootNamesFor(r) IN
    LET r1 == Step(r, r0) IN
    LET r2 == Step(r, r1) IN
    LET r3 == Step(r, r2) IN
    LET r4 == Step(r, r3) IN
    LET r5 == Step(r, r4) IN
        r5

DeadFor(r) ==
    CandidateNamesFor(r) \ ReachFor(r)

ResponseFor(r) ==
    LET reason == ReasonFor(r) IN
    LET ok == reason = "none" IN
        [ request_id |-> r.id,
          outcome |-> CASE ok -> "ok"
                    [] reason = "no_symbols" -> "soft_fail"
                    [] OTHER -> "rejected",
          reason |-> reason,
          project_id |-> IF ok THEN ProjectFor(r) ELSE 0,
          limit |-> IF reason = "invalid_project" THEN 0 ELSE LimitFor(r),
          include_tests |-> r.include_tests,
          include_bare |-> r.include_bare,
          roots |-> IF ok THEN RootNamesFor(r) ELSE {},
          reached |-> IF ok THEN ReachFor(r) ELSE {},
          dead |-> IF ok THEN DeadFor(r) ELSE {},
          accepted_edges |-> IF ok THEN AcceptedEdgesFor(r) ELSE {},
          effect_project_id |-> IF ok THEN ProjectFor(r) ELSE 0,
          bfs_rounds |-> IF ok THEN 5 ELSE 0,
          writes |-> 0,
          lock_held |-> FALSE ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      reason: Reasons,
      project_id: ProjectIds,
      limit: 0..1000,
      include_tests: BOOLEAN,
      include_bare: BOOLEAN,
      roots: SUBSET Names,
      reached: SUBSET Names,
      dead: SUBSET Names,
      accepted_edges: SUBSET Edges,
      effect_project_id: ProjectIds,
      bfs_rounds: 0..5,
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
        /\ response.roots = {}
        /\ response.dead = {}

NoSymbolsSoftFail ==
    ReasonFor(req) = "no_symbols" =>
        /\ response.outcome = "soft_fail"
        /\ response.roots = {}
        /\ response.dead = {}

LimitBounded ==
    response.outcome # "rejected" => response.limit \in 1..1000

TestRootsOptIn ==
    response.outcome = "ok" /\ ~req.include_tests =>
        /\ "test_entry" \notin response.roots
        /\ "test_only" \notin response.reached

BareNameOptIn ==
    response.outcome = "ok" /\ ~req.include_bare =>
        "via_bare" \notin response.reached

AcceptedEdgesSameProject ==
    \A e \in response.accepted_edges :
        /\ e.source_project = response.project_id
        /\ e.source_file_project = response.project_id
        /\ e.target_project = response.project_id
        /\ e.target_file_project = response.project_id

AcceptedEdgesClosedKind ==
    \A e \in response.accepted_edges :
        e.kind \in {"exact_in_file", "exact_via_import", "bare_name_in_project"}

StaleEdgesExcluded ==
    response.outcome = "ok" =>
        /\ "foreign" \notin response.reached
        /\ "foreign" \notin response.dead

DeadCandidatesProjectScoped ==
    response.outcome = "ok" =>
        response.dead \subseteq CandidateNamesFor(req)

EffectEnrichmentUsesResolvedProject ==
    response.outcome = "ok" => response.effect_project_id = response.project_id

BoundedTraversal ==
    response.outcome = "ok" => response.bfs_rounds <= Cardinality(Names)

ReadOnlyNoHeldLock ==
    /\ response.writes = 0
    /\ response.lock_held = FALSE

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidProjectsReject /\
        NoSymbolsSoftFail /\
        LimitBounded /\
        TestRootsOptIn /\
        BareNameOptIn /\
        AcceptedEdgesSameProject /\
        AcceptedEdgesClosedKind /\
        StaleEdgesExcluded /\
        DeadCandidatesProjectScoped /\
        EffectEnrichmentUsesResolvedProject /\
        BoundedTraversal /\
        ReadOnlyNoHeldLock)

================================================================================
