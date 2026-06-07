---------------------------- MODULE QualityReportBoundary ----------------------------
(***************************************************************************)
(* `quality_report` request boundary and side-effect discipline.            *)
(*                                                                         *)
(* The MCP wrapper validates local parameters, resolves one normalized      *)
(* project id, optionally runs a bounded list of cron refreshes, aggregates *)
(* the report for that resolved id, and persists one best-effort history row*)
(* for the same id.                                                         *)
(*                                                                         *)
(* Verified:                                                               *)
(*   - bad local inputs reject before project lookup or cron execution;     *)
(*   - missing/blank/duplicate projects reject before cron/aggregate/write; *)
(*   - successful calls resolve the project exactly once;                   *)
(*   - refresh_crons and trend_points are explicitly bounded;               *)
(*   - successful calls aggregate and persist history for one identity;     *)
(*   - response envelopes expose the canonical output format.               *)
(***************************************************************************)

EXTENDS Naturals, Sequences

MaxTrend == 120
MaxRefresh == 8

Formats == {"markdown", "org", "latex", "html", "text", "json"}
Severities == {"low", "medium", "high", "critical"}
Reasons ==
    {"none", "bad_format", "bad_severity", "too_many_crons",
     "blank_cron", "blank_project", "missing_project", "duplicate_project"}

NoReq ==
    [ id |-> 0,
      project |-> "",
      format |-> "markdown",
      severity |-> "low",
      trend |-> 12,
      refresh |-> <<>> ]

Requests ==
    { [ id |-> 1, project |-> " graph-proj ", format |-> " md ",
        severity |-> " low ", trend |-> 500, refresh |-> <<>> ],
      [ id |-> 2, project |-> "graph-proj", format |-> "pdf",
        severity |-> "low", trend |-> 12, refresh |-> <<>> ],
      [ id |-> 3, project |-> "graph-proj", format |-> "json",
        severity |-> "info", trend |-> 12, refresh |-> <<>> ],
      [ id |-> 4, project |-> "graph-proj", format |-> "text",
        severity |-> "low", trend |-> 12,
        refresh |-> <<"symbol-extraction", "call-graph", "function-metrics",
                      "graph-analysis", "a2a-reflect", "msm-calibrate",
                      "fuzzy-sync", "symbol-extraction", "call-graph">> ],
      [ id |-> 5, project |-> "graph-proj", format |-> "text",
        severity |-> "low", trend |-> 12, refresh |-> <<"symbol-extraction", "   ">> ],
      [ id |-> 6, project |-> "   ", format |-> "text",
        severity |-> "low", trend |-> 12, refresh |-> <<>> ],
      [ id |-> 7, project |-> "missing-proj", format |-> "text",
        severity |-> "low", trend |-> 12, refresh |-> <<"call-graph">> ],
      [ id |-> 8, project |-> "dupe-proj", format |-> "text",
        severity |-> "low", trend |-> 12, refresh |-> <<"call-graph">> ],
      [ id |-> 9, project |-> "graph-proj", format |-> "html",
        severity |-> "critical", trend |-> 0,
        refresh |-> <<"symbol-extraction", "call-graph">> ] }

TrimProject(p) ==
    CASE p = " graph-proj " -> "graph-proj"
      [] p = "   " -> ""
      [] OTHER -> p

CanonFormat(f) ==
    CASE f = " md " -> "markdown"
      [] f = "md" -> "markdown"
      [] f = "gfm" -> "markdown"
      [] f = "org-mode" -> "org"
      [] f = "tex" -> "latex"
      [] f = "txt" -> "text"
      [] f = "plain" -> "text"
      [] OTHER -> f

CanonSeverity(s) ==
    CASE s = " low " -> "low"
      [] s = "med" -> "medium"
      [] s = "crit" -> "critical"
      [] OTHER -> s

TrimJob(j) ==
    CASE j = "   " -> ""
      [] OTHER -> j

HasBlankJob(refresh) ==
    \E i \in 1..Len(refresh) : TrimJob(refresh[i]) = ""

ProjectState(project) ==
    CASE TrimProject(project) = "" -> "blank_project"
      [] TrimProject(project) = "missing-proj" -> "missing_project"
      [] TrimProject(project) = "dupe-proj" -> "duplicate_project"
      [] OTHER -> "ok"

ClampTrend(n) ==
    IF n > MaxTrend THEN MaxTrend ELSE n

TrendReturned(n) ==
    IF ClampTrend(n) = 0 THEN 0 ELSE ClampTrend(n) + 1

Reject(r, reason, lookups) ==
    [ rejected |-> TRUE,
      reason |-> reason,
      normalized_project |-> TrimProject(r.project),
      canonical_format |-> CanonFormat(r.format),
      effective_trend_points |-> ClampTrend(r.trend),
      project_lookups |-> lookups,
      cron_runs |-> 0,
      aggregate_runs |-> 0,
      history_writes |-> 0,
      trend_returned |-> 0 ]

Accept(r) ==
    [ rejected |-> FALSE,
      reason |-> "none",
      normalized_project |-> TrimProject(r.project),
      canonical_format |-> CanonFormat(r.format),
      effective_trend_points |-> ClampTrend(r.trend),
      project_lookups |-> 1,
      cron_runs |-> Len(r.refresh),
      aggregate_runs |-> 1,
      history_writes |-> 1,
      trend_returned |-> TrendReturned(r.trend) ]

Evaluate(r) ==
    IF ~(CanonFormat(r.format) \in Formats) THEN Reject(r, "bad_format", 0)
    ELSE IF ~(CanonSeverity(r.severity) \in Severities) THEN Reject(r, "bad_severity", 0)
    ELSE IF Len(r.refresh) > MaxRefresh THEN Reject(r, "too_many_crons", 0)
    ELSE IF HasBlankJob(r.refresh) THEN Reject(r, "blank_cron", 0)
    ELSE IF ProjectState(r.project) # "ok" THEN Reject(r, ProjectState(r.project), 1)
    ELSE Accept(r)

NoResp ==
    [ rejected |-> FALSE,
      reason |-> "none",
      normalized_project |-> "",
      canonical_format |-> "markdown",
      effective_trend_points |-> 12,
      project_lookups |-> 0,
      cron_runs |-> 0,
      aggregate_runs |-> 0,
      history_writes |-> 0,
      trend_returned |-> 0 ]

VARIABLES req, resp

vars == <<req, resp>>

Init ==
    /\ req = NoReq
    /\ resp = NoResp

Handle(r) ==
    /\ req = NoReq
    /\ r \in Requests
    /\ req' = r
    /\ resp' = Evaluate(r)

Done ==
    /\ req # NoReq
    /\ UNCHANGED vars

Next ==
    \/ \E r \in Requests : Handle(r)
    \/ Done

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ resp.rejected \in BOOLEAN
    /\ resp.reason \in Reasons
    /\ resp.normalized_project \in {"", "graph-proj", "missing-proj", "dupe-proj"}
    /\ resp.canonical_format \in Formats \cup {"pdf"}
    /\ resp.effective_trend_points \in 0..MaxTrend
    /\ resp.project_lookups \in 0..1
    /\ resp.cron_runs \in 0..MaxRefresh
    /\ resp.aggregate_runs \in 0..1
    /\ resp.history_writes \in 0..1
    /\ resp.trend_returned \in 0..(MaxTrend + 1)

LocalRejectsHaveNoSideEffects ==
    req # NoReq /\ resp.reason \in {"bad_format", "bad_severity", "too_many_crons", "blank_cron"} =>
        /\ resp.project_lookups = 0
        /\ resp.cron_runs = 0
        /\ resp.aggregate_runs = 0
        /\ resp.history_writes = 0

ProjectRejectsBeforeWork ==
    req # NoReq /\ resp.reason \in {"blank_project", "missing_project", "duplicate_project"} =>
        /\ resp.project_lookups = 1
        /\ resp.cron_runs = 0
        /\ resp.aggregate_runs = 0
        /\ resp.history_writes = 0

SuccessfulCallsUseOneProjectIdentity ==
    req # NoReq /\ ~resp.rejected =>
        /\ resp.normalized_project = "graph-proj"
        /\ resp.project_lookups = 1
        /\ resp.aggregate_runs = 1
        /\ resp.history_writes = 1

RefreshCronRunsBounded ==
    req # NoReq =>
        /\ resp.cron_runs <= MaxRefresh
        /\ (resp.cron_runs > 0 => ~resp.rejected /\ resp.project_lookups = 1)

TrendPointsBounded ==
    req # NoReq =>
        /\ resp.effective_trend_points <= MaxTrend
        /\ resp.trend_returned <= MaxTrend + 1
        /\ (resp.effective_trend_points = 0 => resp.trend_returned = 0)

CanonicalEnvelopeFormat ==
    req # NoReq /\ ~resp.rejected =>
        resp.canonical_format \in Formats

=============================================================================
