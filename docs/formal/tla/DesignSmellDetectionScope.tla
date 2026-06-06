-------------------------- MODULE DesignSmellDetectionScope --------------------------
(***************************************************************************)
(* `design_smell_detection` request and metric-scope boundary.             *)
(***************************************************************************)

EXTENDS Naturals, Integers, Sequences, FiniteSets

MaxLimit == 1000
AllowedSmells ==
    {"god_class", "srp_violation", "shotgun_surgery",
     "stale_module", "unstable_dependency"}
Outcomes == {"ok", "rejected"}

Projects ==
    { [id |-> 1, name |-> "graph"],
      [id |-> 2, name |-> "dup"],
      [id |-> 3, name |-> "dup"] }

ProjectIds == {p.id : p \in Projects}

MetricRows ==
    { [file |-> "util.rs", file_project |-> 1, metric_project |-> 1,
       in_degree |-> 10, churn |-> 8],
      \* Stale row: file belongs to project 1, metric claims project 2.
      [file |-> "api.rs", file_project |-> 1, metric_project |-> 2,
       in_degree |-> 99, churn |-> 99] }

NoReq == [id |-> 0, project |-> "", detect_all |-> TRUE,
          smells |-> {}, limit |-> 30]

Requests ==
    { [id |-> 1, project |-> "graph", detect_all |-> TRUE,
       smells |-> {}, limit |-> 30],
      [id |-> 2, project |-> "graph", detect_all |-> FALSE,
       smells |-> {"unstable_dependency"}, limit |-> -5],
      [id |-> 3, project |-> "graph", detect_all |-> FALSE,
       smells |-> {"mystery"}, limit |-> 30],
      [id |-> 4, project |-> "graph", detect_all |-> FALSE,
       smells |-> {}, limit |-> 30],
      [id |-> 5, project |-> "dup", detect_all |-> TRUE,
       smells |-> {}, limit |-> 30] }

RequestIds == {r.id : r \in Requests}

ProjectMatches(name) == {p \in Projects : p.name = name}

ResolvedProjectId(r) ==
    IF Cardinality(ProjectMatches(r.project)) = 1
    THEN (CHOOSE p \in ProjectMatches(r.project) : TRUE).id
    ELSE 0

NormalizeLimit(n) ==
    IF n < 1 THEN 1 ELSE IF n > MaxLimit THEN MaxLimit ELSE n

RequestedSmell(r, smell) == r.detect_all \/ smell \in r.smells

RequestAccepted(r) ==
    LET pid == ResolvedProjectId(r) IN
    /\ pid # 0
    /\ (r.detect_all \/ r.smells # {})
    /\ r.smells \subseteq AllowedSmells

SmellRecord ==
    [ smell: AllowedSmells,
      file: {"util.rs", "api.rs"},
      project_id: ProjectIds ]

CandidateMetrics(r, pid) ==
    {m \in MetricRows :
        /\ m.file_project = pid
        /\ m.metric_project = pid
        /\ m.in_degree > 5
        /\ m.churn > 2
        /\ RequestedSmell(r, "unstable_dependency") }

CandidateSmells(r, pid) ==
    { [smell |-> "unstable_dependency", file |-> m.file, project_id |-> pid] :
        m \in CandidateMetrics(r, pid) }

Min(a, b) == IF a <= b THEN a ELSE b

ResponseFor(r) ==
    LET pid == ResolvedProjectId(r) IN
    LET accepted == RequestAccepted(r) IN
    LET smells == IF accepted THEN CandidateSmells(r, pid) ELSE {} IN
    [ request_id |-> r.id,
      outcome |-> IF accepted THEN "ok" ELSE "rejected",
      project_id |-> pid,
      limit |-> NormalizeLimit(r.limit),
      smells |-> smells,
      reported_smell_count |-> Min(Cardinality(smells), NormalizeLimit(r.limit)) ]

RequestFor(id) == CHOOSE r \in Requests : r.id = id

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      project_id: ProjectIds \cup {0},
      limit: 1..MaxLimit,
      smells: SUBSET SmellRecord,
      reported_smell_count: 0..MaxLimit ]

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
            /\ responses[i].smells = {}

DuplicateProjectsReject ==
    \A i \in 1..Len(responses) :
        LET r == RequestFor(responses[i].request_id) IN
        Cardinality(ProjectMatches(r.project)) > 1 =>
            responses[i].outcome = "rejected"

SmellsStayInResolvedProject ==
    \A i \in 1..Len(responses) :
        \A s \in responses[i].smells : s.project_id = responses[i].project_id

StaleMetricRowsIgnored ==
    \A i \in 1..Len(responses) :
        \A s \in responses[i].smells : s.file # "api.rs"

OnlyRequestedSmellsReturned ==
    \A i \in 1..Len(responses) :
        LET r == RequestFor(responses[i].request_id) IN
        \A s \in responses[i].smells : RequestedSmell(r, s.smell)

LimitBounded ==
    \A i \in 1..Len(responses) :
        /\ responses[i].limit \in 1..MaxLimit
        /\ responses[i].reported_smell_count <= responses[i].limit

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidRequestsRejectNoRows /\
        DuplicateProjectsReject /\
        SmellsStayInResolvedProject /\
        StaleMetricRowsIgnored /\
        OnlyRequestedSmellsReturned /\
        LimitBounded)

================================================================================
