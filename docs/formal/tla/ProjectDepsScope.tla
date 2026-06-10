----------------------------- MODULE ProjectDepsScope -----------------------------
(***************************************************************************)
(* Request and row-scope boundary for the cross-project dependency-edge    *)
(* read tools: project_dependents and project_dependencies.                *)
(*                                                                         *)
(* Both tools resolve exactly one project display name fail-closed         *)
(* (`sota_helpers::project_id_or_err` — trim, reject blank / unknown /     *)
(* DUPLICATE), then load the LIVE dependency edges (`valid_to IS NULL`)    *)
(* for that one resolved project id:                                       *)
(*   - project_dependents  -> reverse edges (rows where                    *)
(*       dependency_project_id = resolved id);                             *)
(*   - project_dependencies -> forward edges (rows where                   *)
(*       dependent_project_id = resolved id).                              *)
(*                                                                         *)
(* Obligations: blank/duplicate/unknown -> rejected, no edges; accepted    *)
(* edges are the live edges incident on exactly the one resolved id, in    *)
(* the correct direction; superseded (closed `valid_to`) edges never       *)
(* appear; the tools are read-only.                                        *)
(*                                                                         *)
(* One request is processed per behavior (CircularDependenciesScope shape) *)
(* so the state space stays small and finite.                              *)
(***************************************************************************)

EXTENDS Naturals, Integers, Sequences, FiniteSets

Outcomes == {"ok", "rejected"}

\* "a" / "b" / "c" are unique; "dup" is a duplicate display-name pair the
\* resolver must reject.
Projects ==
    { [id |-> 1, name |-> "a"],
      [id |-> 2, name |-> "b"],
      [id |-> 3, name |-> "c"],
      [id |-> 4, name |-> "dup"],
      [id |-> 5, name |-> "dup"] }

ProjectIds == {p.id : p \in Projects}

\* Directed dependency edges: `dependent` depends ON `dependency`.
\* `live` models the `valid_to IS NULL` predicate — a superseded edge has
\* live = FALSE and must never be returned. Edge 5 is a stale (closed) edge.
Edges ==
    { [eid |-> 1, dependent |-> 1, dependency |-> 2, live |-> TRUE],
      [eid |-> 2, dependent |-> 1, dependency |-> 3, live |-> TRUE],
      [eid |-> 3, dependent |-> 2, dependency |-> 3, live |-> TRUE],
      [eid |-> 4, dependent |-> 3, dependency |-> 1, live |-> TRUE],
      [eid |-> 5, dependent |-> 1, dependency |-> 2, live |-> FALSE] }

EdgeIds == {e.eid : e \in Edges}

\* The live edge sets the two queries actually load, keyed by a project id.
LiveDependentsOf(pid) ==
    {e.eid : e \in {e2 \in Edges : e2.live /\ e2.dependency = pid}}
LiveDependenciesOf(pid) ==
    {e.eid : e \in {e2 \in Edges : e2.live /\ e2.dependent = pid}}

Tools == {"dependents", "dependencies"}

\* Name inputs include blank, duplicate, unknown, padded-but-valid, and valid.
Requests ==
    { [id |-> 1, tool |-> "dependents",   project |-> "a"],
      [id |-> 2, tool |-> "dependents",   project |-> "dup"],
      [id |-> 3, tool |-> "dependents",   project |-> "  "],
      [id |-> 4, tool |-> "dependents",   project |-> "missing"],
      [id |-> 5, tool |-> "dependents",   project |-> " c "],
      [id |-> 6, tool |-> "dependencies", project |-> "a"],
      [id |-> 7, tool |-> "dependencies", project |-> "dup"],
      [id |-> 8, tool |-> "dependencies", project |-> "  "],
      [id |-> 9, tool |-> "dependencies", project |-> "b"] }

RequestIds == {r.id : r \in Requests}

Trim(s) ==
    CASE s = "  "  -> ""
      [] s = " c " -> "c"
      [] OTHER     -> s

ProjectMatches(name) == {p \in Projects : p.name = name}

ResolveId(name) ==
    LET t == Trim(name) IN
    IF t = "" THEN 0
    ELSE IF Cardinality(ProjectMatches(t)) = 1
         THEN (CHOOSE p \in ProjectMatches(t) : TRUE).id
         ELSE 0

NameRejected(name) == ResolveId(name) = 0

EdgesFor(r, pid) ==
    IF r.tool = "dependents" THEN LiveDependentsOf(pid) ELSE LiveDependenciesOf(pid)

ResponseFor(r) ==
    LET rejected == NameRejected(r.project) IN
    LET pid == ResolveId(r.project) IN
    [ request_id |-> r.id,
      tool       |-> r.tool,
      outcome    |-> IF rejected THEN "rejected" ELSE "ok",
      project_id |-> IF rejected THEN 0 ELSE pid,
      edges      |-> IF rejected THEN {} ELSE EdgesFor(r, pid) ]

ResponseRecord ==
    [ request_id: RequestIds,
      tool: Tools,
      outcome: Outcomes,
      project_id: ProjectIds \cup {0},
      edges: SUBSET EdgeIds ]

VARIABLES req, response

vars == <<req, response>>

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests
    /\ response \in ResponseRecord
    /\ response.request_id = req.id

\* Blank/duplicate/unknown project names fail closed with no edges.
BlankOrDuplicateProjectRejects ==
    NameRejected(req.project) =>
        /\ response.outcome = "rejected"
        /\ response.edges = {}
        /\ response.project_id = 0

\* Every returned edge is a LIVE edge (no superseded `valid_to` row leaks).
OnlyLiveEdges ==
    \A eid \in response.edges :
        \E e \in Edges : e.eid = eid /\ e.live

\* dependents returns exactly the live REVERSE edges of the one resolved id.
DependentsEdgesScoped ==
    (req.tool = "dependents" /\ response.outcome = "ok") =>
        /\ response.edges = LiveDependentsOf(response.project_id)
        /\ \A eid \in response.edges :
              \E e \in Edges :
                  e.eid = eid /\ e.live /\ e.dependency = response.project_id

\* dependencies returns exactly the live FORWARD edges of the one resolved id.
DependenciesEdgesScoped ==
    (req.tool = "dependencies" /\ response.outcome = "ok") =>
        /\ response.edges = LiveDependenciesOf(response.project_id)
        /\ \A eid \in response.edges :
              \E e \in Edges :
                  e.eid = eid /\ e.live /\ e.dependent = response.project_id

\* On success the project id is a real resolved id (never the 0 sentinel).
ResolvedProjectIdReal ==
    response.outcome = "ok" => response.project_id \in ProjectIds

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        BlankOrDuplicateProjectRejects /\
        OnlyLiveEdges /\
        DependentsEdgesScoped /\
        DependenciesEdgesScoped /\
        ResolvedProjectIdReal)

================================================================================
