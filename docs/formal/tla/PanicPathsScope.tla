------------------------------ MODULE PanicPathsScope ------------------------------
(***************************************************************************)
(* `panic_paths` request/scoping model.                                    *)
(*                                                                         *)
(* The tool resolves one project, validates the closed entry-filter set,    *)
(* clamps the row limit, and reports only function_metrics rows whose       *)
(* function symbol and indexed file both belong to the resolved project.    *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

Projects == {"p"}
MaxLimit == 3
EntryFilters == {"any", "pub", "module", "private"}

Files ==
    { [id |-> 1, project |-> "p", path |-> "src/a.rs"],
      [id |-> 2, project |-> "other", path |-> "src/foreign.rs"] }

Symbols ==
    { [id |-> 1, file_id |-> 1, visibility |-> "public"],
      [id |-> 2, file_id |-> 2, visibility |-> "public"],
      [id |-> 3, file_id |-> 1, visibility |-> "private"] }

MetricRows ==
    { [function_id |-> 1, file_id |-> 1, metric_project |-> "p", panic_paths |-> 3],
      [function_id |-> 2, file_id |-> 2, metric_project |-> "p", panic_paths |-> 9],
      [function_id |-> 3, file_id |-> 1, metric_project |-> "p", panic_paths |-> 2] }

EffectRows ==
    { [id |-> 1, symbol_id |-> 1, file_project |-> "p"],
      [id |-> 2, symbol_id |-> 2, file_project |-> "other"] }

Requests ==
    { [id |-> 1, project |-> "p", unique_project |-> TRUE, entry_filter |-> "pub",
       raw_limit |-> 99],
      [id |-> 2, project |-> "p", unique_project |-> TRUE, entry_filter |-> "any",
       raw_limit |-> 0],
      [id |-> 3, project |-> "", unique_project |-> FALSE, entry_filter |-> "any",
       raw_limit |-> 10],
      [id |-> 4, project |-> "p", unique_project |-> FALSE, entry_filter |-> "any",
       raw_limit |-> 10],
      [id |-> 5, project |-> "p", unique_project |-> TRUE, entry_filter |-> "public",
       raw_limit |-> 10] }

RequestIds == {r.id : r \in Requests}
SymbolIds == {s.id : s \in Symbols}
EffectIds == {e.id : e \in EffectRows}

FileProject(file_id) ==
    (CHOOSE f \in Files : f.id = file_id).project

SymbolFile(symbol_id) ==
    (CHOOSE s \in Symbols : s.id = symbol_id).file_id

SymbolVisibility(symbol_id) ==
    (CHOOSE s \in Symbols : s.id = symbol_id).visibility

ValidProject(r) ==
    /\ r.project # ""
    /\ r.project \in Projects
    /\ r.unique_project

ValidFilter(r) == r.entry_filter \in EntryFilters

ValidMetricForProject(m, project) ==
    /\ m.metric_project = project
    /\ m.panic_paths > 0
    /\ SymbolFile(m.function_id) = m.file_id
    /\ FileProject(m.file_id) = project

FilterAllows(entry_filter, symbol_id) ==
    CASE entry_filter = "any" -> TRUE
      [] entry_filter = "pub" -> SymbolVisibility(symbol_id) = "public"
      [] entry_filter = "module" -> SymbolVisibility(symbol_id) = "module"
      [] entry_filter = "private" -> SymbolVisibility(symbol_id) = "private"

ClampLimit(v) ==
    IF v < 1 THEN 1 ELSE IF v > MaxLimit THEN MaxLimit ELSE v

CandidateFunctions(r) ==
    {m.function_id : m \in {x \in MetricRows :
        /\ ValidMetricForProject(x, r.project)
        /\ FilterAllows(r.entry_filter, x.function_id)}}

BoundedFunctions(s, limit) ==
    IF Cardinality(s) <= limit
    THEN s
    ELSE CHOOSE t \in SUBSET s : Cardinality(t) = limit

EffectIdsForProject(project) ==
    {e.id : e \in {x \in EffectRows : x.file_project = project}}

Accepted(r) == ValidProject(r) /\ ValidFilter(r)

ResponseFor(r) ==
    LET accepted == Accepted(r) IN
    LET limit == ClampLimit(r.raw_limit) IN
        [ request_id |-> r.id,
          accepted |-> accepted,
          queried |-> accepted,
          entry_filter |-> IF accepted THEN r.entry_filter ELSE "any",
          limit |-> IF accepted THEN limit ELSE 1,
          functions |-> IF accepted THEN BoundedFunctions(CandidateFunctions(r), limit) ELSE {},
          effect_symbols |-> IF accepted THEN EffectIdsForProject(r.project) ELSE {},
          writes |-> 0,
          lock_held |-> FALSE ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      accepted: BOOLEAN,
      queried: BOOLEAN,
      entry_filter: EntryFilters,
      limit: 1..MaxLimit,
      functions: SUBSET SymbolIds,
      effect_symbols: SUBSET EffectIds,
      writes: 0..0,
      lock_held: BOOLEAN ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK == response \in ResponseRecord

InvalidRequestsDoNotQuery ==
    ~Accepted(req) =>
        /\ response.accepted = FALSE
        /\ response.queried = FALSE
        /\ response.functions = {}

LimitsAreBounded ==
    response.accepted => response.limit \in 1..MaxLimit

EntryFilterRespected ==
    response.accepted =>
        \A function_id \in response.functions :
            FilterAllows(response.entry_filter, function_id)

ReportedFunctionsScoped ==
    response.accepted =>
        \A m \in MetricRows :
            m.function_id \in response.functions =>
                ValidMetricForProject(m, req.project)

StaleMetricsRejected ==
    response.accepted =>
        \A m \in MetricRows :
            ~ValidMetricForProject(m, req.project) =>
                m.function_id \notin response.functions

EffectsScoped ==
    response.accepted =>
        \A e \in EffectRows :
            e.id \in response.effect_symbols => e.file_project = req.project

ReadOnlyNoHeldLock ==
    /\ response.writes = 0
    /\ response.lock_held = FALSE

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidRequestsDoNotQuery /\
        LimitsAreBounded /\
        EntryFilterRespected /\
        ReportedFunctionsScoped /\
        StaleMetricsRejected /\
        EffectsScoped /\
        ReadOnlyNoHeldLock)

================================================================================
