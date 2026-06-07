------------------------ MODULE PhoneticSymbolSearchBoundary ------------------------
(***************************************************************************)
(* `phonetic_symbol_search` MCP request boundary.                           *)
(*                                                                         *)
(* The phonetic/edit dictionary internals are verified in liblevenshtein and*)
(* libdictenstein. pgmcp's local obligation is to normalize and reject bad  *)
(* requests before opening the project trie, clamp caller-supplied bounds,  *)
(* use the requested project's vocabulary, and report only rows within the  *)
(* effective phonetic edit distance.                                       *)
(***************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

Projects == {"alpha", "beta"}
Statuses == {"idle", "pending", "done"}
Reasons == {"none", "blank_query", "query_too_large", "blank_project"}

DefaultDistance == 2
MaxDistance == 64
DefaultLimit == 20
MaxLimit == 100
MaxQueryBytes == 512

NoReq ==
    [ id |-> 0,
      query |-> "",
      project |-> "alpha",
      max_distance |-> DefaultDistance,
      limit |-> DefaultLimit ]

Requests ==
    { [id |-> 1, query |-> "   ", project |-> "alpha", max_distance |-> 2, limit |-> 20],
      [id |-> 2, query |-> "too_large", project |-> "alpha", max_distance |-> 2, limit |-> 20],
      [id |-> 3, query |-> "fone", project |-> "   ", max_distance |-> 2, limit |-> 20],
      [id |-> 4, query |-> " fone ", project |-> " alpha ", max_distance |-> 1000, limit |-> 0],
      [id |-> 5, query |-> "backend", project |-> "beta", max_distance |-> 0, limit |-> 20] }

RequestIds == {r.id : r \in Requests}

NormalizeQuery(q) ==
    CASE q = " fone " -> "fone"
      [] q = "   " -> ""
      [] OTHER -> q

NormalizeProject(p) ==
    CASE p = " alpha " -> "alpha"
      [] p = "   " -> ""
      [] OTHER -> p

QueryBytes(q) ==
    CASE q = "too_large" -> MaxQueryBytes + 1
      [] q = "" -> 0
      [] OTHER -> 16

ClampDistance(distance) ==
    IF distance > MaxDistance THEN MaxDistance ELSE distance

ClampLimit(limit) ==
    IF limit < 1 THEN 1 ELSE IF limit > MaxLimit THEN MaxLimit ELSE limit

Vocabulary ==
    { [project |-> "alpha", key |-> "phone_handler", phonetic_distance |-> 1],
      [project |-> "alpha", key |-> "decode_frame", phonetic_distance |-> 5],
      [project |-> "beta", key |-> "backend", phonetic_distance |-> 0],
      [project |-> "beta", key |-> "bakend", phonetic_distance |-> 1] }

VisibleRows(project, distance) ==
    {row \in Vocabulary :
        /\ row.project = project
        /\ row.phonetic_distance <= distance}

RequestFor(id) == CHOOSE r \in Requests : r.id = id

NoResp ==
    [ rejected |-> FALSE,
      reason |-> "none",
      normalized_query |-> "",
      normalized_project |-> "alpha",
      effective_distance |-> DefaultDistance,
      effective_limit |-> DefaultLimit,
      rows |-> {} ]

ResponseRecord ==
    [ rejected: BOOLEAN,
      reason: Reasons,
      normalized_query: {"", "fone", "backend", "too_large"},
      normalized_project: {""} \cup Projects,
      effective_distance: 0..MaxDistance,
      effective_limit: 1..MaxLimit,
      rows: SUBSET Vocabulary ]

VARIABLES req, status, resp

vars == <<req, status, resp>>

Init ==
    /\ req = NoReq
    /\ status = "idle"
    /\ resp = NoResp

PickRequest(r) ==
    /\ status = "idle"
    /\ r \in Requests
    /\ req' = r
    /\ status' = "pending"
    /\ UNCHANGED resp

RejectBlankQuery ==
    /\ status = "pending"
    /\ NormalizeQuery(req.query) = ""
    /\ resp' = [NoResp EXCEPT
        !.rejected = TRUE,
        !.reason = "blank_query",
        !.normalized_query = ""]
    /\ status' = "done"
    /\ UNCHANGED req

RejectOversizedQuery ==
    /\ status = "pending"
    /\ NormalizeQuery(req.query) # ""
    /\ QueryBytes(NormalizeQuery(req.query)) > MaxQueryBytes
    /\ resp' = [NoResp EXCEPT
        !.rejected = TRUE,
        !.reason = "query_too_large",
        !.normalized_query = NormalizeQuery(req.query)]
    /\ status' = "done"
    /\ UNCHANGED req

RejectBlankProject ==
    /\ status = "pending"
    /\ NormalizeQuery(req.query) # ""
    /\ QueryBytes(NormalizeQuery(req.query)) <= MaxQueryBytes
    /\ NormalizeProject(req.project) = ""
    /\ resp' = [NoResp EXCEPT
        !.rejected = TRUE,
        !.reason = "blank_project",
        !.normalized_query = NormalizeQuery(req.query),
        !.normalized_project = ""]
    /\ status' = "done"
    /\ UNCHANGED req

Respond ==
    /\ status = "pending"
    /\ NormalizeQuery(req.query) # ""
    /\ QueryBytes(NormalizeQuery(req.query)) <= MaxQueryBytes
    /\ NormalizeProject(req.project) # ""
    /\ LET distance == ClampDistance(req.max_distance) IN
       LET limit == ClampLimit(req.limit) IN
       LET project == NormalizeProject(req.project) IN
       \E rows \in SUBSET VisibleRows(project, distance) :
          /\ Cardinality(rows) <= limit
          /\ resp' =
              [ rejected |-> FALSE,
                reason |-> "none",
                normalized_query |-> NormalizeQuery(req.query),
                normalized_project |-> project,
                effective_distance |-> distance,
                effective_limit |-> limit,
                rows |-> rows ]
    /\ status' = "done"
    /\ UNCHANGED req

TerminalStutter ==
    /\ status = "done"
    /\ UNCHANGED vars

Next ==
    \/ \E r \in Requests : PickRequest(r)
    \/ RejectBlankQuery
    \/ RejectOversizedQuery
    \/ RejectBlankProject
    \/ Respond
    \/ TerminalStutter

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ status \in Statuses
    /\ resp \in ResponseRecord

BlankQueriesRejected ==
    status = "done" /\ NormalizeQuery(req.query) = "" =>
        /\ resp.rejected
        /\ resp.reason = "blank_query"
        /\ resp.rows = {}

OversizedQueriesRejected ==
    status = "done" /\ NormalizeQuery(req.query) # "" /\ QueryBytes(NormalizeQuery(req.query)) > MaxQueryBytes =>
        /\ resp.rejected
        /\ resp.reason = "query_too_large"
        /\ resp.rows = {}

BlankProjectsRejected ==
    status = "done" /\ NormalizeQuery(req.query) # "" /\ QueryBytes(NormalizeQuery(req.query)) <= MaxQueryBytes /\ NormalizeProject(req.project) = "" =>
        /\ resp.rejected
        /\ resp.reason = "blank_project"
        /\ resp.rows = {}

EffectiveDistanceClamped ==
    status = "done" /\ ~resp.rejected =>
        resp.effective_distance = ClampDistance(req.max_distance)

EffectiveLimitClamped ==
    status = "done" /\ ~resp.rejected =>
        resp.effective_limit = ClampLimit(req.limit)

RowsProjectScoped ==
    status = "done" /\ ~resp.rejected =>
        \A row \in resp.rows : row.project = resp.normalized_project

RowsWithinEffectiveDistance ==
    status = "done" /\ ~resp.rejected =>
        \A row \in resp.rows : row.phonetic_distance <= resp.effective_distance

OutputWithinLimit ==
    status = "done" /\ ~resp.rejected =>
        Cardinality(resp.rows) <= resp.effective_limit

ExactModeDoesNotAdmitTypos ==
    status = "done" /\ ~resp.rejected /\ req.max_distance = 0 =>
        \A row \in resp.rows : row.phonetic_distance = 0

NormalizedInputsUsed ==
    status = "done" =>
        /\ resp.normalized_query = NormalizeQuery(req.query)
        /\ resp.normalized_project = NormalizeProject(req.project)

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        BlankQueriesRejected /\
        OversizedQueriesRejected /\
        BlankProjectsRejected /\
        EffectiveDistanceClamped /\
        EffectiveLimitClamped /\
        RowsProjectScoped /\
        RowsWithinEffectiveDistance /\
        OutputWithinLimit /\
        ExactModeDoesNotAdmitTypos /\
        NormalizedInputsUsed)

================================================================================
