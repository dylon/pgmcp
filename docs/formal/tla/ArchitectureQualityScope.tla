------------------------ MODULE ArchitectureQualityScope ------------------------
(***************************************************************************)
(* `architecture_quality` request boundary.                                *)
(*                                                                         *)
(* The production tool trims project/detail, resolves a unique project id,  *)
(* rejects invalid detail modes, reads metric rows only when file_metrics   *)
(* and indexed_files agree on project identity, excludes stale              *)
(* cross-project import edges, and omits data-absent dimensions from the    *)
(* overall-score denominator.                                               *)
(***************************************************************************)

EXTENDS Naturals, FiniteSets

ProjectIds == {"none", "p1", "p2", "duplicate"}
Details == {"summary", "full", "verbose"}
Files == {"p1.rs", "foreign.rs"}
Edges == {"foreign-edge"}

Requests ==
    { [id |-> 1, raw_project |-> "", raw_detail |-> "summary"],
      [id |-> 2, raw_project |-> "dup", raw_detail |-> "summary"],
      [id |-> 3, raw_project |-> " p ", raw_detail |-> " full "],
      [id |-> 4, raw_project |-> "p", raw_detail |-> "verbose"],
      [id |-> 5, raw_project |-> "p", raw_detail |-> "none"] }

RequestIds == {r.id : r \in Requests}

NormalizeProject(raw) ==
    CASE raw = " p " -> "p"
      [] OTHER -> raw

NormalizeDetail(raw) ==
    CASE raw = " full " -> "full"
      [] raw = "none" -> "summary"
      [] OTHER -> raw

ResolveProject(project) ==
    CASE project = "" -> "none"
      [] project = "p" -> "p1"
      [] project = "dup" -> "duplicate"
      [] OTHER -> "none"

MetricRows ==
    { [file |-> "p1.rs", metric_project |-> "p1", file_project |-> "p1"],
      \* Stale denormalized row: metric_project matches p1, but the file
      \* belongs to p2. It must not feed any architecture-quality dimension.
      [file |-> "foreign.rs", metric_project |-> "p1", file_project |-> "p2"] }

ImportEdges ==
    { [id |-> "foreign-edge", edge_project |-> "p1",
       source_project |-> "p1", target_project |-> "p2"] }

ScopedMetricFiles(project_id) ==
    {m.file : m \in {row \in MetricRows :
        row.metric_project = project_id /\ row.file_project = project_id}}

ScopedEdges(project_id) ==
    {e.id : e \in {edge \in ImportEdges :
        edge.edge_project = project_id
        /\ edge.source_project = project_id
        /\ edge.target_project = project_id}}

ResponseFor(r) ==
    LET project == NormalizeProject(r.raw_project) IN
    LET detail == NormalizeDetail(r.raw_detail) IN
    LET project_id == ResolveProject(project) IN
        CASE project = "" ->
            [ request_id |-> r.id,
              project |-> "",
              project_id |-> "none",
              detail |-> detail,
              rejected |-> TRUE,
              reason |-> "blank",
              metric_files |-> {},
              sdp_edges |-> {},
              dimensions |-> 0,
              present_dimensions |-> 0,
              overall_denominator |-> 0,
              descriptions |-> FALSE,
              writes |-> 0,
              locks |-> 0 ]
          [] ~(detail \in {"summary", "full"}) ->
            [ request_id |-> r.id,
              project |-> project,
              project_id |-> "none",
              detail |-> detail,
              rejected |-> TRUE,
              reason |-> "detail",
              metric_files |-> {},
              sdp_edges |-> {},
              dimensions |-> 0,
              present_dimensions |-> 0,
              overall_denominator |-> 0,
              descriptions |-> FALSE,
              writes |-> 0,
              locks |-> 0 ]
          [] project_id = "duplicate" ->
            [ request_id |-> r.id,
              project |-> project,
              project_id |-> "none",
              detail |-> detail,
              rejected |-> TRUE,
              reason |-> "duplicate",
              metric_files |-> {},
              sdp_edges |-> {},
              dimensions |-> 0,
              present_dimensions |-> 0,
              overall_denominator |-> 0,
              descriptions |-> FALSE,
              writes |-> 0,
              locks |-> 0 ]
          [] OTHER ->
            [ request_id |-> r.id,
              project |-> project,
              project_id |-> project_id,
              detail |-> detail,
              rejected |-> FALSE,
              reason |-> "none",
              metric_files |-> ScopedMetricFiles(project_id),
              sdp_edges |-> ScopedEdges(project_id),
              dimensions |-> 10,
              \* One modeled topic-derived dimension is data-absent and N/A.
              present_dimensions |-> 9,
              overall_denominator |-> 9,
              descriptions |-> detail = "full",
              writes |-> 0,
              locks |-> 0 ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      project: {"", "p", "dup"},
      project_id: ProjectIds,
      detail: Details,
      rejected: BOOLEAN,
      reason: {"none", "blank", "duplicate", "detail"},
      metric_files: SUBSET Files,
      sdp_edges: SUBSET Edges,
      dimensions: 0..10,
      present_dimensions: 0..10,
      overall_denominator: 0..10,
      descriptions: BOOLEAN,
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
    /\ NormalizeDetail(req.raw_detail) \in {"summary", "full"} =>
        /\ response.rejected
        /\ response.reason = "duplicate"
        /\ response.project_id = "none"

InvalidDetailsRejected ==
    ~(NormalizeDetail(req.raw_detail) \in {"summary", "full"}) =>
        /\ response.rejected
        /\ response.reason = "detail"

ProjectOutputNormalized ==
    ~response.rejected /\ req.raw_project = " p " => response.project = "p"

DetailOutputNormalized ==
    ~response.rejected => response.detail \in {"summary", "full"}

FullDetailHasDescriptions ==
    ~response.rejected => (response.descriptions <=> response.detail = "full")

MetricRowsProjectConsistent ==
    ~response.rejected =>
        response.metric_files = ScopedMetricFiles(response.project_id)

StaleMetricRowsExcluded ==
    ~response.rejected /\ response.project_id = "p1" =>
        "foreign.rs" \notin response.metric_files

SdpEdgesProjectConsistent ==
    ~response.rejected =>
        response.sdp_edges = ScopedEdges(response.project_id)

StaleCrossProjectEdgesExcluded ==
    ~response.rejected /\ response.project_id = "p1" =>
        "foreign-edge" \notin response.sdp_edges

AbsentDimensionsExcludedFromMean ==
    ~response.rejected =>
        /\ response.dimensions = 10
        /\ response.present_dimensions <= response.dimensions
        /\ response.overall_denominator = response.present_dimensions

ReadOnlyNoLocks ==
    /\ response.writes = 0
    /\ response.locks = 0

=============================================================================
