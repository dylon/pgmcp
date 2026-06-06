------------------------ MODULE ComplexityHotspotsScoping ------------------------
(***************************************************************************)
(* `complexity_hotspots` request boundary.                                *)
(*                                                                         *)
(* The tool ranks files inside one named project. Correctness hinges on    *)
(* three guards before ranking:                                             *)
(*   - project display-name ambiguity must fail closed;                    *)
(*   - a unique project name must be resolved to an id before querying;     *)
(*   - caller-supplied limits must stay inside a finite cap.               *)
(***************************************************************************)

EXTENDS Integers, Sequences, FiniteSets

Projects ==
    { [id |-> 1, name |-> "unique"],
      [id |-> 2, name |-> "duplicate"],
      [id |-> 3, name |-> "duplicate"] }

Files ==
    { [project_id |-> 1, path |-> "unique/a.rs"],
      [project_id |-> 1, path |-> "unique/b.rs"],
      [project_id |-> 1, path |-> "unique/c.rs"],
      [project_id |-> 2, path |-> "dup-left/a.rs"],
      [project_id |-> 3, path |-> "dup-right/a.rs"] }

NoReq == [id |-> 0, project |-> "", limit |-> 20]

Requests ==
    { [id |-> 1, project |-> "unique", limit |-> -10],
      [id |-> 2, project |-> "unique", limit |-> 0],
      [id |-> 3, project |-> "unique", limit |-> 2],
      [id |-> 4, project |-> "unique", limit |-> 500],
      [id |-> 5, project |-> "duplicate", limit |-> 20],
      [id |-> 6, project |-> "missing", limit |-> 20] }

RequestIds == {r.id : r \in Requests}
ProjectIds == {p.id : p \in Projects}
Outcomes == {"ok", "rejected"}

ClampLimit(limit) ==
    IF limit < 1 THEN 1 ELSE IF limit > 100 THEN 100 ELSE limit

Matches(project_name) == {p \in Projects : p.name = project_name}

ResolvedProjectId(r) ==
    IF Cardinality(Matches(r.project)) = 1
    THEN (CHOOSE p \in Matches(r.project) : TRUE).id
    ELSE 0

VisibleFiles(r) ==
    {f \in Files : f.project_id = ResolvedProjectId(r)}

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      resolved_project_id: ProjectIds \cup {0},
      effective_limit: 1..100,
      rows: SUBSET Files ]

Init ==
    /\ req \in Requests
    /\ LET cap == ClampLimit(req.limit) IN
       IF Cardinality(Matches(req.project)) > 1 THEN
        response =
            [ request_id |-> req.id,
              outcome |-> "rejected",
              resolved_project_id |-> 0,
              effective_limit |-> ClampLimit(req.limit),
              rows |-> {} ]
       ELSE
       \E rows \in SUBSET VisibleFiles(req) :
          /\ Cardinality(rows) <= cap
          /\ response =
              [ request_id |-> req.id,
                outcome |-> "ok",
                resolved_project_id |-> ResolvedProjectId(req),
                effective_limit |-> cap,
                rows |-> rows ]

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests
    /\ response \in ResponseRecord

AmbiguousProjectRejected ==
    Cardinality(Matches(req.project)) > 1 =>
        /\ response.outcome = "rejected"
        /\ response.rows = {}

AcceptedRowsProjectScoped ==
    response.outcome = "ok" =>
        \A row \in response.rows :
            row.project_id = response.resolved_project_id

EffectiveLimitClamped ==
    response.effective_limit = ClampLimit(req.limit)

OutputWithinLimit ==
    Cardinality(response.rows) <= response.effective_limit

MissingProjectReturnsNoRows ==
    Cardinality(Matches(req.project)) = 0 => response.rows = {}

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        AmbiguousProjectRejected /\
        AcceptedRowsProjectScoped /\
        EffectiveLimitClamped /\
        OutputWithinLimit /\
        MissingProjectReturnsNoRows)

=============================================================================
