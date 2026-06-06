------------------------------- MODULE McpToolTelemetryFilters -------------------------------
(***************************************************************************)
(* `mcp_tool_telemetry` request boundary.                                  *)
(*                                                                         *)
(* The tool normalizes optional filters once, clamps lookback/raw limits,  *)
(* validates the aggregation mode, and reuses the normalized filters in    *)
(* every SQL aggregation over mcp_tool_calls.                              *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

MaxSince == 44640
MaxRawLimit == 1000

Aggregations ==
    {"summary", "top_tools", "top_callers", "top_projects", "error_rate", "histogram", "raw"}

Rows ==
    { [id |-> 1, tool |-> "semantic_search", client |-> "cli", project |-> "pgmcp", duration |-> 10, outcome |-> "ok"],
      [id |-> 2, tool |-> "grep", client |-> "cli", project |-> "pgmcp", duration |-> 20, outcome |-> "error"],
      [id |-> 3, tool |-> "semantic_search", client |-> "cli", project |-> "other", duration |-> 30, outcome |-> "ok"],
      [id |-> 4, tool |-> "grep", client |-> "claude-code", project |-> "other", duration |-> 40, outcome |-> "ok"],
      [id |-> 5, tool |-> "orient", client |-> "cli", project |-> "", duration |-> 50, outcome |-> "ok"] }

Requests ==
    { [id |-> 1, aggregation |-> "", tool |-> "", client |-> "", project |-> "", since |-> 60, limit |-> 100],
      [id |-> 2, aggregation |-> " top_tools ", tool |-> "", client |-> "", project |-> " pgmcp ", since |-> 60, limit |-> 100],
      [id |-> 3, aggregation |-> "top_callers", tool |-> " grep ", client |-> "", project |-> " pgmcp ", since |-> -5, limit |-> 100],
      [id |-> 4, aggregation |-> "top_projects", tool |-> "semantic_search", client |-> " cli ", project |-> "", since |-> 50000, limit |-> 100],
      [id |-> 5, aggregation |-> "histogram", tool |-> "semantic_search", client |-> "", project |-> "pgmcp", since |-> 60, limit |-> 100],
      [id |-> 6, aggregation |-> "raw", tool |-> "", client |-> "", project |-> "pgmcp", since |-> 60, limit |-> -10],
      [id |-> 7, aggregation |-> "raw", tool |-> "", client |-> "", project |-> "pgmcp", since |-> 60, limit |-> 5000],
      [id |-> 8, aggregation |-> "bogus", tool |-> "", client |-> "", project |-> "pgmcp", since |-> 60, limit |-> 100] }

RequestIds == {r.id : r \in Requests}
Outcomes == {"ok", "rejected"}
Reasons == {"none", "invalid_aggregation"}
FilterValues == {"none", "semantic_search", "grep", "orient", "cli", "claude-code", "pgmcp", "other"}

NormalizeAggregation(raw) ==
    CASE raw = "" -> "summary"
      [] raw = " top_tools " -> "top_tools"
      [] OTHER -> raw

NormalizeOptional(raw) ==
    CASE raw = "" -> "none"
      [] raw = " grep " -> "grep"
      [] raw = " cli " -> "cli"
      [] raw = " pgmcp " -> "pgmcp"
      [] OTHER -> raw

ClampSince(since) ==
    IF since < 1 THEN 1 ELSE IF since > MaxSince THEN MaxSince ELSE since

ClampRawLimit(limit) ==
    IF limit < 1 THEN 1 ELSE IF limit > MaxRawLimit THEN MaxRawLimit ELSE limit

MatchesFilters(row, r) ==
    /\ (NormalizeOptional(r.tool) = "none" \/ row.tool = NormalizeOptional(r.tool))
    /\ (NormalizeOptional(r.client) = "none" \/ row.client = NormalizeOptional(r.client))
    /\ (NormalizeOptional(r.project) = "none" \/ row.project = NormalizeOptional(r.project))

FilteredRows(r) ==
    {row \in Rows : MatchesFilters(row, r)}

AggregationRows(r) ==
    IF NormalizeAggregation(r.aggregation) = "top_projects" THEN
        {row \in FilteredRows(r) : row.project # ""}
    ELSE
        FilteredRows(r)

BoundedRawRows(r) ==
    LET rows == AggregationRows(r) IN
    LET cap == ClampRawLimit(r.limit) IN
    IF Cardinality(rows) <= cap THEN rows
    ELSE {CHOOSE row \in rows : TRUE}

ResponseRows(r) ==
    IF NormalizeAggregation(r.aggregation) = "raw" THEN BoundedRawRows(r)
    ELSE AggregationRows(r)

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      reason: Reasons,
      aggregation: Aggregations,
      tool_filter: FilterValues,
      client_filter: FilterValues,
      project_filter: FilterValues,
      since_minutes: 1..MaxSince,
      raw_limit: 1..MaxRawLimit,
      rows: SUBSET Rows ]

Init ==
    /\ req \in Requests
    /\ LET aggregation == NormalizeAggregation(req.aggregation) IN
       LET since == ClampSince(req.since) IN
       LET raw_limit == ClampRawLimit(req.limit) IN
       IF ~(aggregation \in Aggregations) THEN
        response =
            [ request_id |-> req.id,
              outcome |-> "rejected",
              reason |-> "invalid_aggregation",
              aggregation |-> "summary",
              tool_filter |-> NormalizeOptional(req.tool),
              client_filter |-> NormalizeOptional(req.client),
              project_filter |-> NormalizeOptional(req.project),
              since_minutes |-> since,
              raw_limit |-> raw_limit,
              rows |-> {} ]
       ELSE
        response =
            [ request_id |-> req.id,
              outcome |-> "ok",
              reason |-> "none",
              aggregation |-> aggregation,
              tool_filter |-> NormalizeOptional(req.tool),
              client_filter |-> NormalizeOptional(req.client),
              project_filter |-> NormalizeOptional(req.project),
              since_minutes |-> since,
              raw_limit |-> raw_limit,
              rows |-> ResponseRows(req) ]

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests
    /\ response \in ResponseRecord

InvalidAggregationRejected ==
    ~(NormalizeAggregation(req.aggregation) \in Aggregations) =>
        /\ response.outcome = "rejected"
        /\ response.rows = {}

AggregationNormalizedAndValidated ==
    response.outcome = "ok" =>
        /\ response.aggregation = NormalizeAggregation(req.aggregation)
        /\ response.aggregation \in Aggregations

FiltersNormalized ==
    /\ response.tool_filter = NormalizeOptional(req.tool)
    /\ response.client_filter = NormalizeOptional(req.client)
    /\ response.project_filter = NormalizeOptional(req.project)

SinceClamped ==
    response.since_minutes = ClampSince(req.since)

RawLimitClamped ==
    response.raw_limit = ClampRawLimit(req.limit)

RowsMatchNormalizedFilters ==
    \A row \in response.rows :
        /\ (response.tool_filter = "none" \/ row.tool = response.tool_filter)
        /\ (response.client_filter = "none" \/ row.client = response.client_filter)
        /\ (response.project_filter = "none" \/ row.project = response.project_filter)

TopProjectsExcludeEmptyProject ==
    response.outcome = "ok" /\ response.aggregation = "top_projects" =>
        \A row \in response.rows : row.project # ""

RawOutputWithinLimit ==
    response.outcome = "ok" /\ response.aggregation = "raw" =>
        Cardinality(response.rows) <= response.raw_limit

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidAggregationRejected /\
        AggregationNormalizedAndValidated /\
        FiltersNormalized /\
        SinceClamped /\
        RawLimitClamped /\
        RowsMatchNormalizedFilters /\
        TopProjectsExcludeEmptyProject /\
        RawOutputWithinLimit)

=============================================================================
