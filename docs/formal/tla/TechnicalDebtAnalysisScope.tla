---------------------------- MODULE TechnicalDebtAnalysisScope ----------------------------
(***************************************************************************)
(* `technical_debt_analysis` request boundary.                             *)
(*                                                                         *)
(* The tool resolves one project, scores files inside that project, and    *)
(* optionally counts TODO/FIXME-style debt markers. Its local safety        *)
(* obligations are project scoping, bounded output, duplicate-name          *)
(* rejection, and enrichment using the same resolved project id.            *)
(***************************************************************************)

EXTENDS Integers, Sequences, FiniteSets

Projects ==
    { [id |-> 1, name |-> "unique"],
      [id |-> 2, name |-> "duplicate"],
      [id |-> 3, name |-> "duplicate"] }

Files ==
    { [id |-> 10, project_id |-> 1, path |-> "unique/a.rs", markers |-> 3],
      [id |-> 20, project_id |-> 1, path |-> "unique/b.rs", markers |-> 0],
      [id |-> 30, project_id |-> 1, path |-> "unique/c.rs", markers |-> 1],
      [id |-> 40, project_id |-> 2, path |-> "dup-left/a.rs", markers |-> 5],
      [id |-> 50, project_id |-> 3, path |-> "dup-right/a.rs", markers |-> 5] }

NoReq == [id |-> 0, project |-> "", limit |-> 30, include_todos |-> TRUE]

Requests ==
    { [id |-> 1, project |-> "unique", limit |-> -10, include_todos |-> TRUE],
      [id |-> 2, project |-> "unique", limit |-> 500, include_todos |-> TRUE],
      [id |-> 3, project |-> "unique", limit |-> 2, include_todos |-> FALSE],
      [id |-> 4, project |-> "duplicate", limit |-> 30, include_todos |-> TRUE],
      [id |-> 5, project |-> "missing", limit |-> 30, include_todos |-> TRUE] }

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

BoundedRows(r) ==
    LET visible == VisibleFiles(r) IN
    LET cap == ClampLimit(r.limit) IN
    IF Cardinality(visible) <= cap THEN visible
    ELSE {CHOOSE f \in visible : TRUE}

MarkerCount(r, rows) ==
    IF r.include_todos THEN
        Cardinality({marker \in 1..5 :
            \E row \in rows : marker <= row.markers})
    ELSE 0

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      resolved_project_id: ProjectIds \cup {0},
      effect_project_id: ProjectIds \cup {0},
      effective_limit: 1..100,
      total_debt_markers: 0..5,
      rows: SUBSET Files ]

Init ==
    /\ req \in Requests
    /\ LET cap == ClampLimit(req.limit) IN
       IF Cardinality(Matches(req.project)) # 1 THEN
        response =
            [ request_id |-> req.id,
              outcome |-> "rejected",
              resolved_project_id |-> 0,
              effect_project_id |-> 0,
              effective_limit |-> cap,
              total_debt_markers |-> 0,
              rows |-> {} ]
       ELSE
       LET pid == ResolvedProjectId(req) IN
       LET rows == BoundedRows(req) IN
       /\ Cardinality(rows) <= cap
       /\ response =
           [ request_id |-> req.id,
             outcome |-> "ok",
             resolved_project_id |-> pid,
             effect_project_id |-> pid,
             effective_limit |-> cap,
             total_debt_markers |-> MarkerCount(req, VisibleFiles(req)),
             rows |-> rows ]

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests
    /\ response \in ResponseRecord

NonUniqueProjectRejected ==
    Cardinality(Matches(req.project)) # 1 =>
        /\ response.outcome = "rejected"
        /\ response.rows = {}
        /\ response.resolved_project_id = 0

RowsProjectScoped ==
    \A row \in response.rows :
        row.project_id = response.resolved_project_id

EffectiveLimitClamped ==
    response.effective_limit = ClampLimit(req.limit)

OutputWithinLimit ==
    Cardinality(response.rows) <= response.effective_limit

TodosDisabledSuppressesMarkerCount ==
    ~req.include_todos => response.total_debt_markers = 0

EnrichmentUsesResolvedProject ==
    response.outcome = "ok" =>
        response.effect_project_id = response.resolved_project_id

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        NonUniqueProjectRejected /\
        RowsProjectScoped /\
        EffectiveLimitClamped /\
        OutputWithinLimit /\
        TodosDisabledSuppressesMarkerCount /\
        EnrichmentUsesResolvedProject)

=============================================================================
