---------------------- MODULE ArchitectureViolationsScope ----------------------
(***************************************************************************)
(* `architecture_violations` request boundary.                             *)
(*                                                                         *)
(* The production tool trims project/severity, resolves exactly one project *)
(* id, rejects invalid severities, builds the graph from import edges whose *)
(* source and target files both belong to that project, caps reported       *)
(* violations, and enriches effects with the same resolved project id.      *)
(***************************************************************************)

EXTENDS Naturals, FiniteSets

ProjectIds == {"none", "p1", "p2", "duplicate"}
Severities == {"low", "medium", "high", "critical", "severe"}
EdgeIds == {"cycle-a", "cycle-b", "foreign-a", "foreign-b"}

MaxReported == 500

Requests ==
    { [id |-> 1, raw_project |-> "", raw_severity |-> "medium"],
      [id |-> 2, raw_project |-> "dup", raw_severity |-> "low"],
      [id |-> 3, raw_project |-> " p ", raw_severity |-> " critical "],
      [id |-> 4, raw_project |-> "p", raw_severity |-> "severe"] }

RequestIds == {r.id : r \in Requests}

NormalizeProject(raw) ==
    CASE raw = " p " -> "p"
      [] OTHER -> raw

NormalizeSeverity(raw) ==
    CASE raw = " critical " -> "critical"
      [] OTHER -> raw

ResolveProject(project) ==
    CASE project = "" -> "none"
      [] project = "p" -> "p1"
      [] project = "dup" -> "duplicate"
      [] OTHER -> "none"

ImportEdges ==
    { [id |-> "cycle-a", edge_project |-> "p1",
       source_project |-> "p1", target_project |-> "p1",
       severity |-> "critical"],
      [id |-> "cycle-b", edge_project |-> "p1",
       source_project |-> "p1", target_project |-> "p1",
       severity |-> "critical"],
      \* Stale rows: edge_project matches p1, but one endpoint belongs to p2.
      [id |-> "foreign-a", edge_project |-> "p1",
       source_project |-> "p1", target_project |-> "p2",
       severity |-> "critical"],
      [id |-> "foreign-b", edge_project |-> "p1",
       source_project |-> "p2", target_project |-> "p1",
       severity |-> "critical"] }

SeverityRank(severity) ==
    CASE severity = "critical" -> 4
      [] severity = "high" -> 3
      [] severity = "medium" -> 2
      [] severity = "low" -> 1
      [] OTHER -> 0

ScopedEdges(project_id) ==
    {edge.id : edge \in {e \in ImportEdges :
        e.edge_project = project_id
        /\ e.source_project = project_id
        /\ e.target_project = project_id}}

ReportedEdges(project_id, severity) ==
    {edge.id : edge \in {e \in ImportEdges :
        e.id \in ScopedEdges(project_id)
        /\ SeverityRank(e.severity) >= SeverityRank(severity)}}

Min(a, b) == IF a < b THEN a ELSE b

ResponseFor(r) ==
    LET project == NormalizeProject(r.raw_project) IN
    LET severity == NormalizeSeverity(r.raw_severity) IN
    LET project_id == ResolveProject(project) IN
    LET scoped == ScopedEdges(project_id) IN
    LET reported == ReportedEdges(project_id, severity) IN
        CASE project = "" ->
            [ request_id |-> r.id,
              project |-> "",
              project_id |-> "none",
              severity |-> severity,
              rejected |-> TRUE,
              reason |-> "blank",
              graph_edges |-> {},
              reported_edges |-> {},
              violation_count |-> 0,
              total_violation_count |-> 0,
              truncated |-> FALSE,
              enrichment_project_id |-> "none",
              writes |-> 0,
              locks |-> 0 ]
          [] ~(severity \in {"low", "medium", "high", "critical"}) ->
            [ request_id |-> r.id,
              project |-> project,
              project_id |-> "none",
              severity |-> severity,
              rejected |-> TRUE,
              reason |-> "severity",
              graph_edges |-> {},
              reported_edges |-> {},
              violation_count |-> 0,
              total_violation_count |-> 0,
              truncated |-> FALSE,
              enrichment_project_id |-> "none",
              writes |-> 0,
              locks |-> 0 ]
          [] project_id = "duplicate" ->
            [ request_id |-> r.id,
              project |-> project,
              project_id |-> "none",
              severity |-> severity,
              rejected |-> TRUE,
              reason |-> "duplicate",
              graph_edges |-> {},
              reported_edges |-> {},
              violation_count |-> 0,
              total_violation_count |-> 0,
              truncated |-> FALSE,
              enrichment_project_id |-> "none",
              writes |-> 0,
              locks |-> 0 ]
          [] OTHER ->
            [ request_id |-> r.id,
              project |-> project,
              project_id |-> project_id,
              severity |-> severity,
              rejected |-> FALSE,
              reason |-> "none",
              graph_edges |-> scoped,
              reported_edges |-> reported,
              violation_count |-> Min(Cardinality(reported), MaxReported),
              total_violation_count |-> Cardinality(reported),
              truncated |-> Cardinality(reported) > MaxReported,
              enrichment_project_id |-> project_id,
              writes |-> 0,
              locks |-> 0 ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      project: {"", "p", "dup"},
      project_id: ProjectIds,
      severity: Severities,
      rejected: BOOLEAN,
      reason: {"none", "blank", "duplicate", "severity"},
      graph_edges: SUBSET EdgeIds,
      reported_edges: SUBSET EdgeIds,
      violation_count: 0..MaxReported,
      total_violation_count: 0..4,
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
    /\ NormalizeSeverity(req.raw_severity) \in {"low", "medium", "high", "critical"} =>
        /\ response.rejected
        /\ response.reason = "duplicate"
        /\ response.project_id = "none"

InvalidSeveritiesRejected ==
    ~(NormalizeSeverity(req.raw_severity) \in {"low", "medium", "high", "critical"}) =>
        /\ response.rejected
        /\ response.reason = "severity"

ProjectOutputNormalized ==
    ~response.rejected /\ req.raw_project = " p " => response.project = "p"

SeverityOutputNormalized ==
    ~response.rejected => response.severity \in {"low", "medium", "high", "critical"}

GraphEdgesProjectScoped ==
    ~response.rejected => response.graph_edges = ScopedEdges(response.project_id)

StaleCrossProjectEdgesExcluded ==
    ~response.rejected /\ response.project_id = "p1" =>
        /\ "foreign-a" \notin response.graph_edges
        /\ "foreign-b" \notin response.graph_edges

ReportedViolationsBounded ==
    ~response.rejected =>
        /\ response.violation_count <= MaxReported
        /\ response.violation_count <= response.total_violation_count

CriticalThresholdSuppressesLowerSeverity ==
    ~response.rejected /\ response.severity = "critical" =>
        response.reported_edges = {"cycle-a", "cycle-b"}

EffectEnrichmentUsesResolvedProject ==
    ~response.rejected => response.enrichment_project_id = response.project_id

ReadOnlyNoLocks ==
    /\ response.writes = 0
    /\ response.locks = 0

=============================================================================
