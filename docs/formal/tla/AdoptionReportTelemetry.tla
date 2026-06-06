------------------------------- MODULE AdoptionReportTelemetry -------------------------------
(***************************************************************************)
(* `adoption_report` telemetry collector.                                  *)
(*                                                                         *)
(* The tool reads durable mcp_tool_calls rows for an allowlist of real      *)
(* clients, classifies tools into adoption families, clamps the lookback    *)
(* window, and renders JSON/Markdown. RLM is a strict subset of A2A.        *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

MaxWindow == 44640

Clients == {"claude-code", "codex-mcp-client", "claude-cli", "cli", "unknown"}
RealClients == {"claude-code", "codex-mcp-client", "claude-cli"}
Families == {"a2a", "csm", "memory", "rlm", "workitem"}
Formats == {"json", "markdown"}

Rows ==
    { [id |-> 1, client |-> "claude-code", session |-> "s1", tool |-> "a2a_send_task", age |-> 1],
      [id |-> 2, client |-> "claude-code", session |-> "s1", tool |-> "a2a_pattern_recursive", age |-> 1],
      [id |-> 3, client |-> "claude-code", session |-> "s2", tool |-> "memory_unified_search", age |-> 1],
      [id |-> 4, client |-> "claude-code", session |-> "s3", tool |-> "work_item_create", age |-> 1],
      [id |-> 5, client |-> "claude-code", session |-> "s4", tool |-> "semantic_search", age |-> 1],
      [id |-> 6, client |-> "cli", session |-> "cli1", tool |-> "a2a_send_task", age |-> 1],
      [id |-> 7, client |-> "claude-code", session |-> "old", tool |-> "memory_unified_search", age |-> 120],
      [id |-> 8, client |-> "codex-mcp-client", session |-> "", tool |-> "csm_validate_run", age |-> 1],
      [id |-> 9, client |-> "unknown", session |-> "u1", tool |-> "work_item_claim", age |-> 1] }

Requests ==
    { [id |-> 1, since |-> 60, format |-> " json "],
      [id |-> 2, since |-> -10, format |-> ""],
      [id |-> 3, since |-> 50000, format |-> " md "],
      [id |-> 4, since |-> 60, format |-> "xml"] }

RequestIds == {r.id : r \in Requests}
Outcomes == {"ok", "rejected"}
Reasons == {"none", "invalid_format"}

NormalizeFormat(raw) ==
    CASE raw = "" -> "json"
      [] raw = " json " -> "json"
      [] raw = " md " -> "markdown"
      [] raw = "md" -> "markdown"
      [] OTHER -> raw

ClampWindow(since) ==
    IF since < 1 THEN 1 ELSE IF since > MaxWindow THEN MaxWindow ELSE since

ToolInFamily(tool, family) ==
    CASE family = "a2a" -> tool \in {"a2a_send_task", "a2a_pattern_recursive"}
      [] family = "csm" -> tool = "csm_validate_run"
      [] family = "memory" -> tool \in {"memory_unified_search", "recall_prompts", "search_mandates", "graph_neighbors"}
      [] family = "rlm" -> tool = "a2a_pattern_recursive"
      [] family = "workitem" -> tool \in {"work_item_create", "work_item_claim", "tag_create"}
      [] OTHER -> FALSE

VisibleRows(r) ==
    {row \in Rows : row.client \in RealClients /\ row.age <= ClampWindow(r.since)}

FamilyRows(r, family) ==
    {row \in VisibleRows(r) : ToolInFamily(row.tool, family)}

Sessions(rows) ==
    {row.session : row \in {r \in rows : r.session # ""}}

VARIABLES req, response

vars == <<req, response>>

FamilyStat ==
    [ family: Families,
      calls: Nat,
      sessions: Nat,
      total_calls: Nat ]

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      reason: Reasons,
      window_minutes: 1..MaxWindow,
      format: Formats,
      rows: SUBSET Rows,
      overall_total_calls: Nat,
      family_stats: [Families -> FamilyStat] ]

BuildStats(r) ==
    [family \in Families |->
        [ family |-> family,
          calls |-> Cardinality(FamilyRows(r, family)),
          sessions |-> Cardinality(Sessions(FamilyRows(r, family))),
          total_calls |-> Cardinality(VisibleRows(r)) ]]

Init ==
    /\ req \in Requests
    /\ LET fmt == NormalizeFormat(req.format) IN
       LET window == ClampWindow(req.since) IN
       IF ~(fmt \in Formats) THEN
        response =
            [ request_id |-> req.id,
              outcome |-> "rejected",
              reason |-> "invalid_format",
              window_minutes |-> window,
              format |-> "json",
              rows |-> {},
              overall_total_calls |-> 0,
              family_stats |-> BuildStats([req EXCEPT !.since = 0]) ]
       ELSE
        response =
            [ request_id |-> req.id,
              outcome |-> "ok",
              reason |-> "none",
              window_minutes |-> window,
              format |-> fmt,
              rows |-> VisibleRows(req),
              overall_total_calls |-> Cardinality(VisibleRows(req)),
              family_stats |-> BuildStats(req) ]

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests
    /\ response \in ResponseRecord

FormatValidatedAndNormalized ==
    NormalizeFormat(req.format) \in Formats =>
        /\ response.outcome = "ok"
        /\ response.format = NormalizeFormat(req.format)

InvalidFormatsRejected ==
    ~(NormalizeFormat(req.format) \in Formats) =>
        /\ response.outcome = "rejected"
        /\ response.rows = {}

WindowClamped ==
    response.window_minutes = ClampWindow(req.since)

AllowlistOnly ==
    \A row \in response.rows : row.client \in RealClients

WindowFilterSound ==
    \A row \in response.rows : row.age <= response.window_minutes

OverallTotalMatchesVisibleRows ==
    response.outcome = "ok" => response.overall_total_calls = Cardinality(response.rows)

FamilyCallsSound ==
    response.outcome = "ok" =>
        \A family \in Families :
            response.family_stats[family].calls = Cardinality(FamilyRows(req, family))

FamilySessionsDeduped ==
    response.outcome = "ok" =>
        \A family \in Families :
            response.family_stats[family].sessions = Cardinality(Sessions(FamilyRows(req, family)))

RlmSubsetOfA2a ==
    response.outcome = "ok" =>
        /\ response.family_stats["rlm"].calls <= response.family_stats["a2a"].calls
        /\ response.family_stats["rlm"].sessions <= response.family_stats["a2a"].sessions

ExcludedClientsDoNotContribute ==
    response.outcome = "ok" =>
        \A row \in Rows :
            row.client \notin RealClients => row \notin response.rows

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        FormatValidatedAndNormalized /\
        InvalidFormatsRejected /\
        WindowClamped /\
        AllowlistOnly /\
        WindowFilterSound /\
        OverallTotalMatchesVisibleRows /\
        FamilyCallsSound /\
        FamilySessionsDeduped /\
        RlmSubsetOfA2a /\
        ExcludedClientsDoNotContribute)

=============================================================================
