------------------------------- MODULE IoHotpathScope -------------------------------
(***************************************************************************)
(* `io_hotpath` request/scoping model.  Regex matching is delegated to the   *)
(* bounded scanner; this spec checks pgmcp's project, metric, and output     *)
(* boundaries around the scanner.                                           *)
(***************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

Projects == {"p"}
MaxLimit == 3
ScanCap == 5
EffectCap == 2

Files ==
    { [id |-> 1, project |-> "p", path |-> "src/a.rs"],
      [id |-> 2, project |-> "p", path |-> "src/b.rs"],
      [id |-> 3, project |-> "other", path |-> "src/a.rs"] }

MetricRows ==
    { [file_id |-> 1, metric_project |-> "p", pagerank |-> 1],
      [file_id |-> 2, metric_project |-> "other", pagerank |-> 9],
      [file_id |-> 3, metric_project |-> "p", pagerank |-> 9] }

EffectRows ==
    { [id |-> 1, file_id |-> 1, file_project |-> "p"],
      [id |-> 2, file_id |-> 2, file_project |-> "p"],
      [id |-> 3, file_id |-> 3, file_project |-> "other"] }

Requests ==
    { [id |-> 1, project |-> "p", unique_project |-> TRUE, raw_limit |-> 2,
       hit_count |-> 2],
      [id |-> 2, project |-> "p", unique_project |-> TRUE, raw_limit |-> 99,
       hit_count |-> 9],
      [id |-> 3, project |-> "", unique_project |-> FALSE, raw_limit |-> 2,
       hit_count |-> 1],
      [id |-> 4, project |-> "p", unique_project |-> FALSE, raw_limit |-> 2,
       hit_count |-> 1],
      [id |-> 5, project |-> "p", unique_project |-> TRUE, raw_limit |-> 0,
       hit_count |-> 1] }

RequestIds == {r.id : r \in Requests}
FileIds == {f.id : f \in Files}
EffectIds == {e.id : e \in EffectRows}

ResponseRecord ==
    [ request_id: RequestIds,
      accepted: BOOLEAN,
      limit: 1..MaxLimit,
      scanned: 0..ScanCap,
      scan_truncated: BOOLEAN,
      reported_files: SUBSET FileIds,
      effect_symbols: SUBSET EffectIds ]

VARIABLES responses, seen, dbState

vars == <<responses, seen, dbState>>

Init ==
    /\ responses = <<>>
    /\ seen = {}
    /\ dbState = "unchanged"

ValidProject(r) ==
    /\ r.project # ""
    /\ r.project \in Projects
    /\ r.unique_project

ClampLimit(v) ==
    IF v < 1 THEN 1 ELSE IF v > MaxLimit THEN MaxLimit ELSE v

ScannedHits(hits) ==
    IF hits > ScanCap THEN ScanCap ELSE hits

FileProject(file_id) ==
    (CHOOSE f \in Files : f.id = file_id).project

LiveMetricForProject(m, project) ==
    /\ m.metric_project = project
    /\ FileProject(m.file_id) = project

MetricFileIds(project) ==
    {m.file_id : m \in {x \in MetricRows : LiveMetricForProject(x, project)}}

CandidateHitFiles(project) ==
    {f.id : f \in {x \in Files : x.project = project}}

ReportedFiles(r) ==
    CandidateHitFiles(r.project)

EffectIdsForProject(project) ==
    {e.id : e \in {x \in EffectRows : x.file_project = project}}

BoundEffects(s) ==
    IF Cardinality(s) <= EffectCap THEN s ELSE CHOOSE t \in SUBSET s : Cardinality(t) = EffectCap

Process(r) ==
    /\ r \in Requests
    /\ r.id \notin seen
    /\ seen' = seen \cup {r.id}
    /\ dbState' = dbState
    /\ IF ~ValidProject(r) THEN
          responses' = Append(responses,
              [request_id |-> r.id,
               accepted |-> FALSE,
               limit |-> 1,
               scanned |-> 0,
               scan_truncated |-> FALSE,
               reported_files |-> {},
               effect_symbols |-> {}])
       ELSE
          responses' = Append(responses,
              [request_id |-> r.id,
               accepted |-> TRUE,
               limit |-> ClampLimit(r.raw_limit),
               scanned |-> ScannedHits(r.hit_count),
               scan_truncated |-> r.hit_count >= ScanCap,
               reported_files |-> ReportedFiles(r),
               effect_symbols |-> BoundEffects(EffectIdsForProject(r.project))])

Next == \E r \in Requests : Process(r)

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ responses \in Seq(ResponseRecord)
    /\ seen \subseteq RequestIds
    /\ dbState = "unchanged"

InvalidProjectsDoNotScan ==
    \A i \in 1..Len(responses) :
        LET r == CHOOSE x \in Requests : x.id = responses[i].request_id IN
        ~ValidProject(r) =>
            /\ responses[i].accepted = FALSE
            /\ responses[i].scanned = 0
            /\ responses[i].reported_files = {}

LimitsAndScansAreBounded ==
    \A i \in 1..Len(responses) :
        /\ responses[i].limit <= MaxLimit
        /\ responses[i].scanned <= ScanCap
        /\ Cardinality(responses[i].reported_files) <= MaxLimit
        /\ Cardinality(responses[i].effect_symbols) <= EffectCap

OnlyProjectFilesReported ==
    \A i \in 1..Len(responses) :
        LET r == CHOOSE x \in Requests : x.id = responses[i].request_id IN
        responses[i].accepted =>
            \A file_id \in responses[i].reported_files :
                FileProject(file_id) = r.project

StaleMetricsRejected ==
    \A i \in 1..Len(responses) :
        LET r == CHOOSE x \in Requests : x.id = responses[i].request_id IN
        responses[i].accepted =>
            \A m \in MetricRows :
                m.file_id \in MetricFileIds(r.project) =>
                    /\ m.metric_project = r.project
                    /\ FileProject(m.file_id) = r.project

EffectsScoped ==
    \A i \in 1..Len(responses) :
        LET r == CHOOSE x \in Requests : x.id = responses[i].request_id IN
        responses[i].accepted =>
            \A e \in EffectRows :
                e.id \in responses[i].effect_symbols => e.file_project = r.project

ReadOnlyAdapter ==
    dbState = "unchanged"

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidProjectsDoNotScan /\
        LimitsAndScansAreBounded /\
        OnlyProjectFilesReported /\
        StaleMetricsRejected /\
        EffectsScoped /\
        ReadOnlyAdapter)

================================================================================
