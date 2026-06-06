----------------------------- MODULE DependencyGraphScope -----------------------------
(***************************************************************************)
(* `dependency_graph` request and row-scope boundary.                      *)
(***************************************************************************)

EXTENDS Naturals, Integers, Sequences, FiniteSets

MaxDepth == 8
MaxReportedEdges == 1

Formats == {"summary", "edges", "dot"}
AllowedEdgeTypes == {"import", "co_change", "semantic"}
Outcomes == {"ok", "rejected"}

Projects ==
    { [id |-> 1, name |-> "graph"],
      [id |-> 2, name |-> "dup"],
      [id |-> 3, name |-> "dup"] }

ProjectIds == {p.id : p \in Projects}

Files ==
    { [id |-> 10, project |-> 1, path |-> "core/a.rs"],
      [id |-> 11, project |-> 1, path |-> "core/b.rs"],
      [id |-> 20, project |-> 2, path |-> "foreign/lib.rs"] }

Edges ==
    { [id |-> 1, project |-> 1, source_project |-> 1, target_project |-> 1, edge_type |-> "import"],
      [id |-> 2, project |-> 1, source_project |-> 1, target_project |-> 1, edge_type |-> "import"],
      \* Stale edge: graph project row points at another project's target file.
      [id |-> 3, project |-> 1, source_project |-> 1, target_project |-> 2, edge_type |-> "import"],
      [id |-> 4, project |-> 2, source_project |-> 2, target_project |-> 2, edge_type |-> "semantic"] }

NoReq == [id |-> 0, project |-> "", format |-> "summary",
          edge_types |-> {}, focus |-> "", depth |-> 0]

Requests ==
    { [id |-> 1, project |-> "graph", format |-> "summary",
       edge_types |-> {"import"}, focus |-> "", depth |-> 2],
      [id |-> 2, project |-> "dup", format |-> "summary",
       edge_types |-> {"import"}, focus |-> "", depth |-> 2],
      [id |-> 3, project |-> "graph", format |-> "xml",
       edge_types |-> {"import"}, focus |-> "", depth |-> 2],
      [id |-> 4, project |-> "graph", format |-> "summary",
       edge_types |-> {"calls"}, focus |-> "", depth |-> 2],
      [id |-> 5, project |-> "graph", format |-> "summary",
       edge_types |-> {"import"}, focus |-> "missing.rs", depth |-> 2],
      [id |-> 6, project |-> "graph", format |-> "edges",
       edge_types |-> {"import"}, focus |-> "", depth |-> 99],
      [id |-> 7, project |-> "graph", format |-> "summary",
       edge_types |-> {"import"}, focus |-> "core/a.rs", depth |-> -5] }

RequestIds == {r.id : r \in Requests}

ProjectMatches(name) == {p \in Projects : p.name = name}

ResolvedProjectId(r) ==
    IF Cardinality(ProjectMatches(r.project)) = 1
    THEN (CHOOSE p \in ProjectMatches(r.project) : TRUE).id
    ELSE 0

NormalizeDepth(d) ==
    IF d < 0 THEN 0 ELSE IF d > MaxDepth THEN MaxDepth ELSE d

FocusExists(r, pid) ==
    r.focus = "" \/ \E f \in Files : f.project = pid /\ f.path = r.focus

RequestAccepted(r) ==
    LET pid == ResolvedProjectId(r) IN
    /\ pid # 0
    /\ r.format \in Formats
    /\ r.edge_types # {}
    /\ r.edge_types \subseteq AllowedEdgeTypes
    /\ FocusExists(r, pid)

ScopedEdges(r, pid) ==
    {e \in Edges :
        /\ e.project = pid
        /\ e.source_project = pid
        /\ e.target_project = pid
        /\ e.edge_type \in r.edge_types}

Min(a, b) == IF a <= b THEN a ELSE b

ResponseFor(r) ==
    LET pid == ResolvedProjectId(r) IN
    LET accepted == RequestAccepted(r) IN
    LET edges == IF accepted THEN ScopedEdges(r, pid) ELSE {} IN
    [ request_id |-> r.id,
      outcome |-> IF accepted THEN "ok" ELSE "rejected",
      project_id |-> pid,
      depth |-> NormalizeDepth(r.depth),
      edges |-> edges,
      reported_edge_count |-> Min(Cardinality(edges), MaxReportedEdges) ]

RequestFor(id) == CHOOSE r \in Requests : r.id = id

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      project_id: ProjectIds \cup {0},
      depth: 0..MaxDepth,
      edges: SUBSET Edges,
      reported_edge_count: 0..MaxReportedEdges ]

VARIABLES phase, req, responses, seen

vars == <<phase, req, responses, seen>>

Init ==
    /\ phase = "idle"
    /\ req = NoReq
    /\ responses = <<>>
    /\ seen = {}

PickRequest(r) ==
    /\ phase = "idle"
    /\ r \in Requests
    /\ r.id \notin seen
    /\ req' = r
    /\ responses' = Append(responses, ResponseFor(r))
    /\ seen' = seen \cup {r.id}
    /\ phase' = "done"

Reset ==
    /\ phase = "done"
    /\ req' = NoReq
    /\ phase' = "idle"
    /\ UNCHANGED <<responses, seen>>

Next ==
    \/ \E r \in Requests : PickRequest(r)
    \/ Reset

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ phase \in {"idle", "done"}
    /\ req \in Requests \cup {NoReq}
    /\ responses \in Seq(ResponseRecord)
    /\ seen \subseteq RequestIds

InvalidRequestsRejectNoRows ==
    \A i \in 1..Len(responses) :
        LET r == RequestFor(responses[i].request_id) IN
        ~RequestAccepted(r) =>
            /\ responses[i].outcome = "rejected"
            /\ responses[i].edges = {}

DuplicateProjectsReject ==
    \A i \in 1..Len(responses) :
        LET r == RequestFor(responses[i].request_id) IN
        Cardinality(ProjectMatches(r.project)) > 1 =>
            responses[i].outcome = "rejected"

FocusMissingRejects ==
    \A i \in 1..Len(responses) :
        LET r == RequestFor(responses[i].request_id) IN
        LET pid == ResolvedProjectId(r) IN
        pid # 0 /\ ~FocusExists(r, pid) =>
            responses[i].outcome = "rejected"

EdgesStayInResolvedProject ==
    \A i \in 1..Len(responses) :
        \A e \in responses[i].edges :
            /\ e.project = responses[i].project_id
            /\ e.source_project = responses[i].project_id
            /\ e.target_project = responses[i].project_id

OnlyAllowedEdgeTypesReturned ==
    \A i \in 1..Len(responses) :
        \A e \in responses[i].edges : e.edge_type \in AllowedEdgeTypes

DepthBounded ==
    \A i \in 1..Len(responses) : responses[i].depth \in 0..MaxDepth

ReportedEdgesBounded ==
    \A i \in 1..Len(responses) : responses[i].reported_edge_count <= MaxReportedEdges

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidRequestsRejectNoRows /\
        DuplicateProjectsReject /\
        FocusMissingRejects /\
        EdgesStayInResolvedProject /\
        OnlyAllowedEdgeTypesReturned /\
        DepthBounded /\
        ReportedEdgesBounded)

================================================================================
