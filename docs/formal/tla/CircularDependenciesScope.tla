----------------------------- MODULE CircularDependenciesScope -----------------------------
(***************************************************************************)
(* `circular_dependencies` request boundary.                              *)
(*                                                                         *)
(* The tool resolves one project display name, loads import edges scoped   *)
(* to that project id, and extracts simple cycles up to a caller-supplied  *)
(* maximum length. The safety obligations are:                             *)
(*   - non-unique project names fail closed;                               *)
(*   - signed max lengths are clamped before search;                       *)
(*   - every reported cycle is a closed import cycle inside the resolved   *)
(*     project;                                                           *)
(*   - rejected requests return no cycles.                                 *)
(***************************************************************************)

EXTENDS Integers, Sequences, FiniteSets

Projects ==
    { [id |-> 1, name |-> "unique"],
      [id |-> 2, name |-> "duplicate"],
      [id |-> 3, name |-> "duplicate"] }

Files ==
    { [id |-> 10, project_id |-> 1, path |-> "unique/core/a.rs"],
      [id |-> 20, project_id |-> 1, path |-> "unique/core/b.rs"],
      [id |-> 30, project_id |-> 1, path |-> "unique/core/c.rs"],
      [id |-> 40, project_id |-> 1, path |-> "unique/api.rs"],
      [id |-> 50, project_id |-> 1, path |-> "unique/util.rs"],
      [id |-> 60, project_id |-> 2, path |-> "dup-left/lib.rs"],
      [id |-> 70, project_id |-> 3, path |-> "dup-right/lib.rs"] }

Edges ==
    { [project_id |-> 1, source |-> 10, target |-> 20, edge_type |-> "import"],
      [project_id |-> 1, source |-> 20, target |-> 30, edge_type |-> "import"],
      [project_id |-> 1, source |-> 30, target |-> 10, edge_type |-> "import"],
      [project_id |-> 1, source |-> 40, target |-> 50, edge_type |-> "import"],
      [project_id |-> 1, source |-> 50, target |-> 40, edge_type |-> "import"],
      [project_id |-> 1, source |-> 10, target |-> 40, edge_type |-> "import"],
      [project_id |-> 2, source |-> 60, target |-> 60, edge_type |-> "import"],
      [project_id |-> 3, source |-> 70, target |-> 70, edge_type |-> "import"] }

CycleUniverse ==
    { <<10, 20, 30>>,
      <<40, 50>>,
      <<10, 40>>,
      <<10, 20>>,
      <<60, 70>>,
      <<60>> }

NoReq == [id |-> 0, project |-> "", max_cycle_length |-> 10]

Requests ==
    { [id |-> 1, project |-> "unique", max_cycle_length |-> -10],
      [id |-> 2, project |-> "unique", max_cycle_length |-> 0],
      [id |-> 3, project |-> "unique", max_cycle_length |-> 2],
      [id |-> 4, project |-> "unique", max_cycle_length |-> 3],
      [id |-> 5, project |-> "unique", max_cycle_length |-> 500],
      [id |-> 6, project |-> "duplicate", max_cycle_length |-> 10],
      [id |-> 7, project |-> "missing", max_cycle_length |-> 10] }

RequestIds == {r.id : r \in Requests}
ProjectIds == {p.id : p \in Projects}
Outcomes == {"ok", "rejected"}

ClampMaxCycleLength(max_cycle_length) ==
    IF max_cycle_length < 2 THEN 2
    ELSE IF max_cycle_length > 64 THEN 64
    ELSE max_cycle_length

Matches(project_name) == {p \in Projects : p.name = project_name}

ResolvedProjectId(r) ==
    IF Cardinality(Matches(r.project)) = 1
    THEN (CHOOSE p \in Matches(r.project) : TRUE).id
    ELSE 0

FileProject(file_id) == (CHOOSE f \in Files : f.id = file_id).project_id

EdgeExists(project_id, source, target) ==
    \E e \in Edges :
        /\ e.project_id = project_id
        /\ e.source = source
        /\ e.target = target
        /\ e.edge_type = "import"

SimpleCycle(c) ==
    /\ Len(c) >= 2
    /\ \A i, j \in 1..Len(c) : i # j => c[i] # c[j]

ClosedImportCycle(c, project_id) ==
    /\ SimpleCycle(c)
    /\ \A i \in 1..(Len(c) - 1) : EdgeExists(project_id, c[i], c[i + 1])
    /\ EdgeExists(project_id, c[Len(c)], c[1])

CycleProjectScoped(c, project_id) ==
    \A i \in 1..Len(c) : FileProject(c[i]) = project_id

CandidateCycles(project_id, max_cycle_length) ==
    {c \in CycleUniverse :
        /\ CycleProjectScoped(c, project_id)
        /\ ClosedImportCycle(c, project_id)
        /\ Len(c) <= max_cycle_length}

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      resolved_project_id: ProjectIds \cup {0},
      effective_max_cycle_length: 2..64,
      cycles: SUBSET CycleUniverse ]

Init ==
    /\ req \in Requests
    /\ LET max_len == ClampMaxCycleLength(req.max_cycle_length) IN
       IF Cardinality(Matches(req.project)) # 1 THEN
        response =
            [ request_id |-> req.id,
              outcome |-> "rejected",
              resolved_project_id |-> 0,
              effective_max_cycle_length |-> max_len,
              cycles |-> {} ]
       ELSE
       LET pid == ResolvedProjectId(req) IN
       \E cycles \in SUBSET CandidateCycles(pid, max_len) :
          /\ response =
              [ request_id |-> req.id,
                outcome |-> "ok",
                resolved_project_id |-> pid,
                effective_max_cycle_length |-> max_len,
                cycles |-> cycles ]

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests
    /\ response \in ResponseRecord

NonUniqueProjectRejected ==
    Cardinality(Matches(req.project)) # 1 =>
        /\ response.outcome = "rejected"
        /\ response.cycles = {}
        /\ response.resolved_project_id = 0

EffectiveMaxCycleLengthClamped ==
    response.effective_max_cycle_length = ClampMaxCycleLength(req.max_cycle_length)

ReportedCyclesProjectScoped ==
    \A c \in response.cycles :
        CycleProjectScoped(c, response.resolved_project_id)

ReportedCyclesWithinMax ==
    \A c \in response.cycles :
        Len(c) <= response.effective_max_cycle_length

ReportedCyclesAreClosedImportCycles ==
    \A c \in response.cycles :
        ClosedImportCycle(c, response.resolved_project_id)

NoCyclesOnRejected ==
    response.outcome = "rejected" => response.cycles = {}

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        NonUniqueProjectRejected /\
        EffectiveMaxCycleLengthClamped /\
        ReportedCyclesProjectScoped /\
        ReportedCyclesWithinMax /\
        ReportedCyclesAreClosedImportCycles /\
        NoCyclesOnRejected)

=============================================================================
