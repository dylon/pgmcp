---------------------------- MODULE HotPathAuditScope ----------------------------
(***************************************************************************)
(* `hot_path_audit` request boundary.                                      *)
(*                                                                         *)
(* The production tool trims project, rejects duplicate names, validates    *)
(* finite thresholds, clamps threshold/limit, reads metrics only when       *)
(* file_metrics.project_id agrees with indexed_files.project_id, and        *)
(* enriches effect symbols with the same resolved project id.               *)
(***************************************************************************)

EXTENDS Naturals, FiniteSets

ProjectIds == {"none", "p1", "p2", "duplicate"}
Files == {"good.rs", "stale.rs"}

Requests ==
    { [id |-> 1, raw_project |-> "", raw_threshold |-> 90, threshold_finite |-> TRUE, raw_limit |-> 20],
      [id |-> 2, raw_project |-> "dup", raw_threshold |-> 90, threshold_finite |-> TRUE, raw_limit |-> 20],
      [id |-> 3, raw_project |-> " p ", raw_threshold |-> 200, threshold_finite |-> TRUE, raw_limit |-> 0],
      [id |-> 4, raw_project |-> "p", raw_threshold |-> 90, threshold_finite |-> FALSE, raw_limit |-> 20] }

RequestIds == {r.id : r \in Requests}

NormalizeProject(raw) ==
    CASE raw = " p " -> "p"
      [] OTHER -> raw

ResolveProject(project) ==
    CASE project = "" -> "none"
      [] project = "p" -> "p1"
      [] project = "dup" -> "duplicate"
      [] OTHER -> "none"

ClampThreshold(raw) ==
    CASE raw < 0 -> 0
      [] raw > 100 -> 100
      [] OTHER -> raw

BoundLimit(raw) ==
    CASE raw < 1 -> 1
      [] raw > 1000 -> 1000
      [] OTHER -> raw

MetricRows ==
    { [file |-> "good.rs", file_project |-> "p1", metric_project |-> "p1", pct |-> 100],
      \* Stale row: metric project disagrees with the file's owning project.
      [file |-> "stale.rs", file_project |-> "p1", metric_project |-> "p2", pct |-> 100] }

ScopedHotFiles(project_id, threshold) ==
    {row.file : row \in {m \in MetricRows :
        m.file_project = project_id
        /\ m.metric_project = project_id
        /\ m.pct >= threshold}}

Min(a, b) == IF a < b THEN a ELSE b

ResponseFor(r) ==
    LET project == NormalizeProject(r.raw_project) IN
    LET project_id == ResolveProject(project) IN
    LET threshold == ClampThreshold(r.raw_threshold) IN
    LET limit == BoundLimit(r.raw_limit) IN
    LET rows == ScopedHotFiles(project_id, threshold) IN
        CASE project = "" ->
            [ request_id |-> r.id,
              project |-> "",
              project_id |-> "none",
              threshold |-> threshold,
              threshold_finite |-> r.threshold_finite,
              limit |-> limit,
              rejected |-> TRUE,
              reason |-> "blank",
              hot_files |-> {},
              returned |-> 0,
              enrichment_project_id |-> "none",
              writes |-> 0,
              locks |-> 0 ]
          [] ~r.threshold_finite ->
            [ request_id |-> r.id,
              project |-> project,
              project_id |-> "none",
              threshold |-> threshold,
              threshold_finite |-> r.threshold_finite,
              limit |-> limit,
              rejected |-> TRUE,
              reason |-> "threshold",
              hot_files |-> {},
              returned |-> 0,
              enrichment_project_id |-> "none",
              writes |-> 0,
              locks |-> 0 ]
          [] project_id = "duplicate" ->
            [ request_id |-> r.id,
              project |-> project,
              project_id |-> "none",
              threshold |-> threshold,
              threshold_finite |-> r.threshold_finite,
              limit |-> limit,
              rejected |-> TRUE,
              reason |-> "duplicate",
              hot_files |-> {},
              returned |-> 0,
              enrichment_project_id |-> "none",
              writes |-> 0,
              locks |-> 0 ]
          [] OTHER ->
            [ request_id |-> r.id,
              project |-> project,
              project_id |-> project_id,
              threshold |-> threshold,
              threshold_finite |-> r.threshold_finite,
              limit |-> limit,
              rejected |-> FALSE,
              reason |-> "none",
              hot_files |-> rows,
              returned |-> Min(Cardinality(rows), limit),
              enrichment_project_id |-> project_id,
              writes |-> 0,
              locks |-> 0 ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      project: {"", "p", "dup"},
      project_id: ProjectIds,
      threshold: 0..100,
      threshold_finite: BOOLEAN,
      limit: 1..1000,
      rejected: BOOLEAN,
      reason: {"none", "blank", "duplicate", "threshold"},
      hot_files: SUBSET Files,
      returned: 0..2,
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

NonFiniteThresholdRejected ==
    ~req.threshold_finite => response.rejected /\ response.reason = "threshold"

DuplicateProjectsRejected ==
    ResolveProject(NormalizeProject(req.raw_project)) = "duplicate" /\ req.threshold_finite =>
        /\ response.rejected
        /\ response.reason = "duplicate"

ThresholdAndLimitBounded ==
    /\ response.threshold \in 0..100
    /\ response.limit \in 1..1000

ProjectOutputNormalized ==
    ~response.rejected /\ req.raw_project = " p " => response.project = "p"

MetricRowsProjectConsistent ==
    ~response.rejected =>
        response.hot_files = ScopedHotFiles(response.project_id, response.threshold)

StaleMetricRowsExcluded ==
    ~response.rejected /\ response.project_id = "p1" =>
        "stale.rs" \notin response.hot_files

ReturnedRowsBounded ==
    ~response.rejected => response.returned <= response.limit

EffectEnrichmentUsesResolvedProject ==
    ~response.rejected => response.enrichment_project_id = response.project_id

ReadOnlyNoLocks ==
    /\ response.writes = 0
    /\ response.locks = 0

=============================================================================
