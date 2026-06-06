------------------------------- MODULE CentralityAnalysisScope -------------------------------
(***************************************************************************)
(* `centrality_analysis` request boundary.                                 *)
(*                                                                         *)
(* The tool resolves one project display name, ranks precomputed           *)
(* file_metrics rows, and enriches with effect/cross-project context.      *)
(* Correctness requires duplicate-name fail-closed behavior, bounded       *)
(* output, metric validation, and every data source using the same          *)
(* resolved project id.                                                     *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

MaxLimit == 200

Projects ==
    { [id |-> 1, name |-> "unique"],
      [id |-> 2, name |-> "duplicate"],
      [id |-> 3, name |-> "duplicate"],
      [id |-> 4, name |-> "empty"] }

MetricRows ==
    { [id |-> 10, metric_project_id |-> 1, file_project_id |-> 1, path |-> "core/a.rs"],
      [id |-> 20, metric_project_id |-> 1, file_project_id |-> 1, path |-> "core/b.rs"],
      [id |-> 30, metric_project_id |-> 1, file_project_id |-> 1, path |-> "util/util.rs"],
      \* Drift row: old name-based joins could expose this across projects.
      [id |-> 40, metric_project_id |-> 1, file_project_id |-> 2, path |-> "dup/leak.rs"],
      [id |-> 50, metric_project_id |-> 2, file_project_id |-> 2, path |-> "dup/a.rs"] }

Requests ==
    { [id |-> 1, project |-> "", metric |-> "pagerank", limit |-> 20],
      [id |-> 2, project |-> "   ", metric |-> "pagerank", limit |-> 20],
      [id |-> 3, project |-> " unique ", metric |-> " pagerank ", limit |-> 20],
      [id |-> 4, project |-> "unique", metric |-> "degree", limit |-> -10],
      [id |-> 5, project |-> "unique", metric |-> "all", limit |-> 500],
      [id |-> 6, project |-> "unique", metric |-> "", limit |-> 2],
      [id |-> 7, project |-> "unique", metric |-> "eigenvector", limit |-> 20],
      [id |-> 8, project |-> "duplicate", metric |-> "pagerank", limit |-> 20],
      [id |-> 9, project |-> "missing", metric |-> "pagerank", limit |-> 20],
      [id |-> 10, project |-> "empty", metric |-> "betweenness", limit |-> 20] }

RequestIds == {r.id : r \in Requests}
ProjectIds == {p.id : p \in Projects}
Outcomes == {"ok", "rejected"}
Reasons == {"none", "blank_project", "non_unique_project", "invalid_metric"}
Metrics == {"all", "pagerank", "betweenness", "degree"}

NormalizeProject(raw) ==
    CASE raw = " unique " -> "unique"
      [] raw = "   " -> ""
      [] OTHER -> raw

NormalizeMetric(raw) ==
    CASE raw = " pagerank " -> "pagerank"
      [] raw = "" -> "all"
      [] OTHER -> raw

ClampLimit(limit) ==
    IF limit < 1 THEN 1 ELSE IF limit > MaxLimit THEN MaxLimit ELSE limit

Matches(project_name) == {p \in Projects : p.name = project_name}

ResolvedProjectId(r) ==
    LET project == NormalizeProject(r.project) IN
    IF Cardinality(Matches(project)) = 1
    THEN (CHOOSE p \in Matches(project) : TRUE).id
    ELSE 0

VisibleRows(r) ==
    LET pid == ResolvedProjectId(r) IN
        {row \in MetricRows : row.metric_project_id = pid /\ row.file_project_id = pid}

BoundedRows(r) ==
    LET visible == VisibleRows(r) IN
    LET cap == ClampLimit(r.limit) IN
    IF Cardinality(visible) <= cap THEN visible
    ELSE {CHOOSE row \in visible : TRUE}

EffectProjectId(r) == ResolvedProjectId(r)
CrossProjectLookupProjectId(r) == ResolvedProjectId(r)

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      reason: Reasons,
      project: {"", "unique", "duplicate", "missing", "empty"},
      metric: Metrics,
      resolved_project_id: ProjectIds \cup {0},
      metric_project_id: ProjectIds \cup {0},
      effect_project_id: ProjectIds \cup {0},
      cross_project_lookup_project_id: ProjectIds \cup {0},
      effective_limit: 1..MaxLimit,
      rows: SUBSET MetricRows ]

Init ==
    /\ req \in Requests
    /\ LET project == NormalizeProject(req.project) IN
       LET metric == NormalizeMetric(req.metric) IN
       LET cap == ClampLimit(req.limit) IN
       IF project = "" THEN
        response =
            [ request_id |-> req.id,
              outcome |-> "rejected",
              reason |-> "blank_project",
              project |-> project,
              metric |-> IF metric \in Metrics THEN metric ELSE "all",
              resolved_project_id |-> 0,
              metric_project_id |-> 0,
              effect_project_id |-> 0,
              cross_project_lookup_project_id |-> 0,
              effective_limit |-> cap,
              rows |-> {} ]
       ELSE IF ~(metric \in Metrics) THEN
        response =
            [ request_id |-> req.id,
              outcome |-> "rejected",
              reason |-> "invalid_metric",
              project |-> project,
              metric |-> "all",
              resolved_project_id |-> 0,
              metric_project_id |-> 0,
              effect_project_id |-> 0,
              cross_project_lookup_project_id |-> 0,
              effective_limit |-> cap,
              rows |-> {} ]
       ELSE IF Cardinality(Matches(project)) # 1 THEN
        response =
            [ request_id |-> req.id,
              outcome |-> "rejected",
              reason |-> "non_unique_project",
              project |-> project,
              metric |-> metric,
              resolved_project_id |-> 0,
              metric_project_id |-> 0,
              effect_project_id |-> 0,
              cross_project_lookup_project_id |-> 0,
              effective_limit |-> cap,
              rows |-> {} ]
       ELSE
       LET pid == ResolvedProjectId(req) IN
       LET rows == BoundedRows(req) IN
       /\ Cardinality(rows) <= cap
       /\ response =
           [ request_id |-> req.id,
             outcome |-> "ok",
             reason |-> "none",
             project |-> project,
             metric |-> metric,
             resolved_project_id |-> pid,
             metric_project_id |-> pid,
             effect_project_id |-> EffectProjectId(req),
             cross_project_lookup_project_id |-> CrossProjectLookupProjectId(req),
             effective_limit |-> cap,
             rows |-> rows ]

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests
    /\ response \in ResponseRecord

InvalidRequestsRejected ==
    (NormalizeProject(req.project) = "" \/
     ~(NormalizeMetric(req.metric) \in Metrics) \/
     Cardinality(Matches(NormalizeProject(req.project))) # 1) =>
        /\ response.outcome = "rejected"
        /\ response.rows = {}
        /\ response.resolved_project_id = 0

MetricValidatedAndNormalized ==
    response.outcome = "ok" =>
        /\ response.metric = NormalizeMetric(req.metric)
        /\ response.metric \in Metrics

ProjectNormalized ==
    response.project = NormalizeProject(req.project)

RowsProjectScoped ==
    \A row \in response.rows :
        /\ row.metric_project_id = response.resolved_project_id
        /\ row.file_project_id = response.resolved_project_id

NoCrossProjectMetricFileDrift ==
    \A row \in response.rows : row.metric_project_id = row.file_project_id

EffectiveLimitClamped ==
    response.effective_limit = ClampLimit(req.limit)

OutputWithinLimit ==
    Cardinality(response.rows) <= response.effective_limit

EnrichmentUsesResolvedProject ==
    response.outcome = "ok" =>
        /\ response.metric_project_id = response.resolved_project_id
        /\ response.effect_project_id = response.resolved_project_id
        /\ response.cross_project_lookup_project_id = response.resolved_project_id

EmptyProjectReturnsJsonEnvelope ==
    response.outcome = "ok" /\ NormalizeProject(req.project) = "empty" =>
        /\ response.rows = {}
        /\ response.resolved_project_id = 4
        /\ response.reason = "none"

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidRequestsRejected /\
        MetricValidatedAndNormalized /\
        ProjectNormalized /\
        RowsProjectScoped /\
        NoCrossProjectMetricFileDrift /\
        EffectiveLimitClamped /\
        OutputWithinLimit /\
        EnrichmentUsesResolvedProject /\
        EmptyProjectReturnsJsonEnvelope)

=============================================================================
