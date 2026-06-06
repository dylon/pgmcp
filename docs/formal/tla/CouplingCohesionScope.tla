------------------------ MODULE CouplingCohesionScope ------------------------
(***************************************************************************)
(* `coupling_cohesion_report` request boundary.                            *)
(*                                                                         *)
(* The production tool trims project/sort fields, clamps module depth,      *)
(* resolves exactly one project id, builds module metrics only from import  *)
(* edges whose source and target files belong to that project, caps module  *)
(* output, and enriches effects with the same resolved project id.          *)
(***************************************************************************)

EXTENDS Naturals, FiniteSets

ProjectIds == {"none", "p1", "p2", "duplicate"}
SortModes == {"distance", "instability", "coupling", "cohesion", "weight"}
EdgeIds == {"src-edge", "foreign-a", "foreign-b"}
Modules == {"src", "foreign"}

MaxModules == 2000

Requests ==
    { [id |-> 1, raw_project |-> "", raw_depth |-> 2, raw_sort |-> "distance"],
      [id |-> 2, raw_project |-> "dup", raw_depth |-> 2, raw_sort |-> "distance"],
      [id |-> 3, raw_project |-> " p ", raw_depth |-> 0 - 5, raw_sort |-> " coupling "],
      [id |-> 4, raw_project |-> "p", raw_depth |-> 99, raw_sort |-> "weight"] }

RequestIds == {r.id : r \in Requests}

NormalizeProject(raw) ==
    CASE raw = " p " -> "p"
      [] OTHER -> raw

NormalizeSort(raw) ==
    CASE raw = " coupling " -> "coupling"
      [] OTHER -> raw

ResolveProject(project) ==
    CASE project = "" -> "none"
      [] project = "p" -> "p1"
      [] project = "dup" -> "duplicate"
      [] OTHER -> "none"

ClampDepth(raw) ==
    CASE raw < 1 -> 1
      [] raw > 8 -> 8
      [] OTHER -> raw

ImportEdges ==
    { [id |-> "src-edge", edge_project |-> "p1",
       source_project |-> "p1", target_project |-> "p1",
       source_module |-> "src", target_module |-> "src"],
      \* Stale rows: edge_project matches p1, but one endpoint belongs to p2.
      [id |-> "foreign-a", edge_project |-> "p1",
       source_project |-> "p1", target_project |-> "p2",
       source_module |-> "src", target_module |-> "foreign"],
      [id |-> "foreign-b", edge_project |-> "p1",
       source_project |-> "p2", target_project |-> "p1",
       source_module |-> "foreign", target_module |-> "src"] }

ScopedEdges(project_id) ==
    {edge.id : edge \in {e \in ImportEdges :
        e.edge_project = project_id
        /\ e.source_project = project_id
        /\ e.target_project = project_id}}

ScopedModules(project_id) ==
    {edge.source_module : edge \in {e \in ImportEdges : e.id \in ScopedEdges(project_id)}}
    \cup {edge.target_module : edge \in {e \in ImportEdges : e.id \in ScopedEdges(project_id)}}

Min(a, b) == IF a < b THEN a ELSE b

ResponseFor(r) ==
    LET project == NormalizeProject(r.raw_project) IN
    LET sort == NormalizeSort(r.raw_sort) IN
    LET project_id == ResolveProject(project) IN
    LET depth == ClampDepth(r.raw_depth) IN
    LET edges == ScopedEdges(project_id) IN
    LET modules == ScopedModules(project_id) IN
        CASE project = "" ->
            [ request_id |-> r.id,
              project |-> "",
              project_id |-> "none",
              module_depth |-> depth,
              sort_by |-> sort,
              rejected |-> TRUE,
              reason |-> "blank",
              graph_edges |-> {},
              modules |-> {},
              module_count |-> 0,
              total_module_count |-> 0,
              truncated |-> FALSE,
              enrichment_project_id |-> "none",
              writes |-> 0,
              locks |-> 0 ]
          [] ~(sort \in {"distance", "instability", "coupling", "cohesion"}) ->
            [ request_id |-> r.id,
              project |-> project,
              project_id |-> "none",
              module_depth |-> depth,
              sort_by |-> sort,
              rejected |-> TRUE,
              reason |-> "sort",
              graph_edges |-> {},
              modules |-> {},
              module_count |-> 0,
              total_module_count |-> 0,
              truncated |-> FALSE,
              enrichment_project_id |-> "none",
              writes |-> 0,
              locks |-> 0 ]
          [] project_id = "duplicate" ->
            [ request_id |-> r.id,
              project |-> project,
              project_id |-> "none",
              module_depth |-> depth,
              sort_by |-> sort,
              rejected |-> TRUE,
              reason |-> "duplicate",
              graph_edges |-> {},
              modules |-> {},
              module_count |-> 0,
              total_module_count |-> 0,
              truncated |-> FALSE,
              enrichment_project_id |-> "none",
              writes |-> 0,
              locks |-> 0 ]
          [] OTHER ->
            [ request_id |-> r.id,
              project |-> project,
              project_id |-> project_id,
              module_depth |-> depth,
              sort_by |-> sort,
              rejected |-> FALSE,
              reason |-> "none",
              graph_edges |-> edges,
              modules |-> modules,
              module_count |-> Min(Cardinality(modules), MaxModules),
              total_module_count |-> Cardinality(modules),
              truncated |-> Cardinality(modules) > MaxModules,
              enrichment_project_id |-> project_id,
              writes |-> 0,
              locks |-> 0 ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      project: {"", "p", "dup"},
      project_id: ProjectIds,
      module_depth: 1..8,
      sort_by: SortModes,
      rejected: BOOLEAN,
      reason: {"none", "blank", "duplicate", "sort"},
      graph_edges: SUBSET EdgeIds,
      modules: SUBSET Modules,
      module_count: 0..MaxModules,
      total_module_count: 0..2,
      truncated: BOOLEAN,
      enrichment_project_id: ProjectIds,
      writes: 0..0,
      locks: 0..0 ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK == response \in ResponseRecord

BlankProjectsRejected ==
    NormalizeProject(req.raw_project) = "" => response.rejected /\ response.reason = "blank"

DuplicateProjectsRejected ==
    ResolveProject(NormalizeProject(req.raw_project)) = "duplicate"
    /\ NormalizeSort(req.raw_sort) \in {"distance", "instability", "coupling", "cohesion"} =>
        /\ response.rejected
        /\ response.reason = "duplicate"
        /\ response.project_id = "none"

InvalidSortRejected ==
    ~(NormalizeSort(req.raw_sort) \in {"distance", "instability", "coupling", "cohesion"}) =>
        /\ response.rejected
        /\ response.reason = "sort"

ModuleDepthClamped ==
    response.module_depth \in 1..8

ProjectOutputNormalized ==
    ~response.rejected /\ req.raw_project = " p " => response.project = "p"

SortOutputNormalized ==
    ~response.rejected => response.sort_by \in {"distance", "instability", "coupling", "cohesion"}

GraphEdgesProjectScoped ==
    ~response.rejected => response.graph_edges = ScopedEdges(response.project_id)

StaleCrossProjectEdgesExcluded ==
    ~response.rejected /\ response.project_id = "p1" =>
        /\ "foreign-a" \notin response.graph_edges
        /\ "foreign-b" \notin response.graph_edges
        /\ "foreign" \notin response.modules

ModuleOutputBounded ==
    ~response.rejected =>
        /\ response.module_count <= MaxModules
        /\ response.module_count <= response.total_module_count

EffectEnrichmentUsesResolvedProject ==
    ~response.rejected => response.enrichment_project_id = response.project_id

ReadOnlyNoLocks ==
    /\ response.writes = 0
    /\ response.locks = 0

=============================================================================
