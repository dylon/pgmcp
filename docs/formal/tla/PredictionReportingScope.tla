------------------------------- MODULE PredictionReportingScope -------------------------------
(***************************************************************************)
(* `code_on_fire` and `documented_tech_debt` request boundaries.           *)
(*                                                                         *)
(* Both tools resolve a single project id, normalize string parameters,     *)
(* clamp signed result limits, and read only rows scoped to that resolved   *)
(* project. `code_on_fire` additionally rejects invalid modes/quartiles     *)
(* and requires metric rows to agree with the resolved project.             *)
(* `documented_tech_debt` validates report filters and returns findings     *)
(* that satisfy the normalized filters.                                     *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

NoQuartile == -999
NoMinAge == -999

Tools == {"code_on_fire", "documented_tech_debt"}
Outcomes == {"ok", "rejected"}
Reasons ==
    {"none", "blank_project", "project_resolution", "invalid_mode",
     "invalid_quartile", "invalid_format", "invalid_category",
     "invalid_severity", "negative_min_age"}
CodeModes == {"intersect", "union", "max"}
Formats == {"summary", "full"}
Categories == {"all", "comments", "stub_macros", "deprecated"}
FindingCategories == {"comment", "stub_macro", "deprecated"}
Severities == {"high", "medium", "low"}
Languages == {"", "rust", "python"}

Projects ==
    { [id |-> 1, name |-> "unique"],
      [id |-> 2, name |-> "duplicate"],
      [id |-> 3, name |-> "duplicate"] }

ProjectIds == {p.id : p \in Projects}
ProjectNames == {p.name : p \in Projects} \cup {"missing", ""}

FunctionMetricRows ==
    { [id |-> 1, file_project_id |-> 1, metric_project_id |-> 1, function |-> "hot"],
      [id |-> 2, file_project_id |-> 1, metric_project_id |-> 2, function |-> "stale_metric"],
      [id |-> 3, file_project_id |-> 2, metric_project_id |-> 2, function |-> "duplicate_hot"],
      [id |-> 4, file_project_id |-> 3, metric_project_id |-> 3, function |-> "duplicate_other"] }

DebtFindings ==
    { [id |-> 1, project_id |-> 1, language |-> "rust", category |-> "comment",
        kind |-> "TODO", severity |-> "medium"],
      [id |-> 2, project_id |-> 1, language |-> "rust", category |-> "comment",
        kind |-> "FIXME", severity |-> "high"],
      [id |-> 3, project_id |-> 1, language |-> "rust", category |-> "deprecated",
        kind |-> "DEPRECATION_ATTR", severity |-> "medium"],
      [id |-> 4, project_id |-> 2, language |-> "python", category |-> "stub_macro",
        kind |-> "PYTHON_NOT_IMPLEMENTED", severity |-> "high"],
      [id |-> 5, project_id |-> 3, language |-> "rust", category |-> "comment",
        kind |-> "TODO", severity |-> "medium"] }

Requests ==
    { [id |-> 1, tool |-> "code_on_fire", project |-> " unique ", limit |-> 500,
        mode |-> " union ", churn_q |-> 0, complexity_q |-> 0, format |-> "",
        category |-> "", severity |-> "", kind |-> "", min_age |-> NoMinAge,
        language |-> ""],
      [id |-> 2, tool |-> "code_on_fire", project |-> "unique", limit |-> -5,
        mode |-> "", churn_q |-> NoQuartile, complexity_q |-> NoQuartile,
        format |-> "", category |-> "", severity |-> "", kind |-> "",
        min_age |-> NoMinAge, language |-> ""],
      [id |-> 3, tool |-> "code_on_fire", project |-> "unique", limit |-> 30,
        mode |-> "sideways", churn_q |-> NoQuartile, complexity_q |-> NoQuartile,
        format |-> "", category |-> "", severity |-> "", kind |-> "",
        min_age |-> NoMinAge, language |-> ""],
      [id |-> 4, tool |-> "code_on_fire", project |-> "unique", limit |-> 30,
        mode |-> "max", churn_q |-> 101, complexity_q |-> 50, format |-> "",
        category |-> "", severity |-> "", kind |-> "", min_age |-> NoMinAge,
        language |-> ""],
      [id |-> 5, tool |-> "code_on_fire", project |-> "duplicate", limit |-> 30,
        mode |-> "max", churn_q |-> 50, complexity_q |-> 50, format |-> "",
        category |-> "", severity |-> "", kind |-> "", min_age |-> NoMinAge,
        language |-> ""],
      [id |-> 6, tool |-> "documented_tech_debt", project |-> " unique ",
        limit |-> 5000, mode |-> "", churn_q |-> NoQuartile,
        complexity_q |-> NoQuartile, format |-> " full ", category |-> " comments ",
        severity |-> " HIGH ", kind |-> "", min_age |-> NoMinAge,
        language |-> " rust "],
      [id |-> 7, tool |-> "documented_tech_debt", project |-> "unique",
        limit |-> -10, mode |-> "", churn_q |-> NoQuartile,
        complexity_q |-> NoQuartile, format |-> "", category |-> "",
        severity |-> "", kind |-> " todo ", min_age |-> NoMinAge,
        language |-> ""],
      [id |-> 8, tool |-> "documented_tech_debt", project |-> "unique",
        limit |-> 100, mode |-> "", churn_q |-> NoQuartile,
        complexity_q |-> NoQuartile, format |-> "xml", category |-> "",
        severity |-> "", kind |-> "", min_age |-> NoMinAge, language |-> ""],
      [id |-> 9, tool |-> "documented_tech_debt", project |-> "unique",
        limit |-> 100, mode |-> "", churn_q |-> NoQuartile,
        complexity_q |-> NoQuartile, format |-> "", category |-> "misc",
        severity |-> "", kind |-> "", min_age |-> NoMinAge, language |-> ""],
      [id |-> 10, tool |-> "documented_tech_debt", project |-> "unique",
        limit |-> 100, mode |-> "", churn_q |-> NoQuartile,
        complexity_q |-> NoQuartile, format |-> "", category |-> "",
        severity |-> "urgent", kind |-> "", min_age |-> NoMinAge,
        language |-> ""],
      [id |-> 11, tool |-> "documented_tech_debt", project |-> "unique",
        limit |-> 100, mode |-> "", churn_q |-> NoQuartile,
        complexity_q |-> NoQuartile, format |-> "", category |-> "",
        severity |-> "", kind |-> "", min_age |-> -1, language |-> ""],
      [id |-> 12, tool |-> "documented_tech_debt", project |-> "missing",
        limit |-> 100, mode |-> "", churn_q |-> NoQuartile,
        complexity_q |-> NoQuartile, format |-> "", category |-> "",
        severity |-> "", kind |-> "", min_age |-> NoMinAge, language |-> ""] }

RequestIds == {r.id : r \in Requests}

Trim(s) ==
    CASE s = " unique " -> "unique"
      [] s = " union " -> "union"
      [] s = " full " -> "full"
      [] s = " comments " -> "comments"
      [] s = " HIGH " -> "HIGH"
      [] s = " rust " -> "rust"
      [] s = " todo " -> "todo"
      [] OTHER -> s

Lower(s) ==
    CASE s = "HIGH" -> "high"
      [] s = "MEDIUM" -> "medium"
      [] s = "LOW" -> "low"
      [] OTHER -> s

Upper(s) ==
    CASE s = "todo" -> "TODO"
      [] s = "fixme" -> "FIXME"
      [] s = "deprecated" -> "DEPRECATED"
      [] OTHER -> s

ModeFor(raw) ==
    LET t == Trim(raw) IN IF t = "" THEN "intersect" ELSE t

FormatFor(raw) ==
    LET t == Trim(raw) IN IF t = "" THEN "summary" ELSE t

CategoryFor(raw) ==
    LET t == Trim(raw) IN IF t = "" THEN "all" ELSE t

SeverityFor(raw) ==
    LET t == Trim(raw) IN IF t = "" THEN "" ELSE Lower(t)

KindFor(raw) ==
    LET t == Trim(raw) IN IF t = "" THEN "" ELSE Upper(t)

LanguageFor(raw) == Trim(raw)

Matches(project_name) == {p \in Projects : p.name = Trim(project_name)}

ResolvedProjectId(r) ==
    IF Cardinality(Matches(r.project)) = 1
    THEN (CHOOSE p \in Matches(r.project) : TRUE).id
    ELSE 0

ClampCodeLimit(limit) ==
    IF limit < 1 THEN 1 ELSE IF limit > 200 THEN 200 ELSE limit

ClampDebtLimit(limit) ==
    IF limit < 1 THEN 1 ELSE IF limit > 1000 THEN 1000 ELSE limit

QuartileFor(raw, default) ==
    IF raw \in 0..100 THEN raw ELSE default

CategoryMatches(finding, category) ==
    CASE category = "all" -> TRUE
      [] category = "comments" -> finding.category = "comment"
      [] category = "stub_macros" -> finding.category = "stub_macro"
      [] category = "deprecated" -> finding.category = "deprecated"
      [] OTHER -> FALSE

CodeRowsFor(r) ==
    LET pid == ResolvedProjectId(r) IN
    {row \in FunctionMetricRows :
        /\ row.file_project_id = pid
        /\ row.metric_project_id = pid}

DebtRowsFor(r) ==
    LET pid == ResolvedProjectId(r) IN
    LET category == CategoryFor(r.category) IN
    LET severity == SeverityFor(r.severity) IN
    LET kind == KindFor(r.kind) IN
    LET language == LanguageFor(r.language) IN
    {finding \in DebtFindings :
        /\ finding.project_id = pid
        /\ CategoryMatches(finding, category)
        /\ (severity = "" \/ finding.severity = severity)
        /\ (kind = "" \/ finding.kind = kind)
        /\ (language = "" \/ finding.language = language)}

BoundedCodeRows(r) ==
    LET rows == CodeRowsFor(r) IN
    LET cap == ClampCodeLimit(r.limit) IN
    IF Cardinality(rows) <= cap THEN rows
    ELSE {CHOOSE row \in rows : TRUE}

BoundedDebtRows(r) ==
    LET rows == DebtRowsFor(r) IN
    LET cap == ClampDebtLimit(r.limit) IN
    IF Cardinality(rows) <= cap THEN rows
    ELSE {CHOOSE row \in rows : TRUE}

ReasonFor(r) ==
    LET project == Trim(r.project) IN
    LET mode == ModeFor(r.mode) IN
    LET format == FormatFor(r.format) IN
    LET category == CategoryFor(r.category) IN
    LET severity == SeverityFor(r.severity) IN
        CASE project = "" -> "blank_project"
          [] Cardinality(Matches(r.project)) # 1 -> "project_resolution"
          [] r.tool = "code_on_fire" /\ ~(mode \in CodeModes) -> "invalid_mode"
          [] r.tool = "code_on_fire" /\
                ~(QuartileFor(r.churn_q, 75) \in 0..100 /\
                  QuartileFor(r.complexity_q, 75) \in 0..100 /\
                  (r.churn_q = NoQuartile \/ r.churn_q \in 0..100) /\
                  (r.complexity_q = NoQuartile \/ r.complexity_q \in 0..100))
             -> "invalid_quartile"
          [] r.tool = "documented_tech_debt" /\ ~(format \in Formats) -> "invalid_format"
          [] r.tool = "documented_tech_debt" /\ ~(category \in Categories) -> "invalid_category"
          [] r.tool = "documented_tech_debt" /\ severity # "" /\ ~(severity \in Severities)
             -> "invalid_severity"
          [] r.tool = "documented_tech_debt" /\ r.min_age # NoMinAge /\ r.min_age < 0
             -> "negative_min_age"
          [] OTHER -> "none"

ResponseFor(r) ==
    LET reason == ReasonFor(r) IN
    LET pid == ResolvedProjectId(r) IN
    LET ok == reason = "none" IN
        [ request_id |-> r.id,
          tool |-> r.tool,
          outcome |-> IF ok THEN "ok" ELSE "rejected",
          reason |-> reason,
          project |-> Trim(r.project),
          resolved_project_id |-> IF ok THEN pid ELSE 0,
          effect_project_id |-> IF ok THEN pid ELSE 0,
          effective_limit |->
              IF r.tool = "code_on_fire" THEN ClampCodeLimit(r.limit)
              ELSE ClampDebtLimit(r.limit),
          mode |-> ModeFor(r.mode),
          churn_q |-> QuartileFor(r.churn_q, 75),
          complexity_q |-> QuartileFor(r.complexity_q, 75),
          format |-> FormatFor(r.format),
          category |-> CategoryFor(r.category),
          severity |-> SeverityFor(r.severity),
          kind |-> KindFor(r.kind),
          language |-> LanguageFor(r.language),
          code_rows |-> IF ok /\ r.tool = "code_on_fire" THEN BoundedCodeRows(r) ELSE {},
          debt_findings |-> IF ok /\ r.tool = "documented_tech_debt" THEN BoundedDebtRows(r) ELSE {} ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      tool: Tools,
      outcome: Outcomes,
      reason: Reasons,
      project: ProjectNames,
      resolved_project_id: ProjectIds \cup {0},
      effect_project_id: ProjectIds \cup {0},
      effective_limit: 1..1000,
      mode: CodeModes \cup {"", "sideways"},
      churn_q: 0..100,
      complexity_q: 0..100,
      format: Formats \cup {"xml"},
      category: Categories \cup {"misc"},
      severity: Severities \cup {"", "urgent"},
      kind: {"", "TODO", "FIXME", "DEPRECATED"},
      language: Languages,
      code_rows: SUBSET FunctionMetricRows,
      debt_findings: SUBSET DebtFindings ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests
    /\ response \in ResponseRecord

UniqueProjectRequired ==
    Trim(req.project) = "" \/ Cardinality(Matches(req.project)) # 1 =>
        /\ response.outcome = "rejected"
        /\ response.resolved_project_id = 0
        /\ response.code_rows = {}
        /\ response.debt_findings = {}

RejectedRequestsDoNotReturnRows ==
    response.outcome = "rejected" =>
        /\ response.code_rows = {}
        /\ response.debt_findings = {}

CodeModeValidated ==
    req.tool = "code_on_fire" /\ response.outcome = "ok" =>
        /\ response.mode \in CodeModes
        /\ response.mode = ModeFor(req.mode)

CodeQuartilesValidated ==
    req.tool = "code_on_fire" /\ response.outcome = "ok" =>
        /\ response.churn_q \in 0..100
        /\ response.complexity_q \in 0..100
        /\ (req.churn_q = NoQuartile \/ req.churn_q \in 0..100)
        /\ (req.complexity_q = NoQuartile \/ req.complexity_q \in 0..100)

CodeLimitClamped ==
    req.tool = "code_on_fire" =>
        response.effective_limit = ClampCodeLimit(req.limit)

CodeMetricRowsProjectConsistent ==
    req.tool = "code_on_fire" =>
        \A row \in response.code_rows :
            /\ row.file_project_id = response.resolved_project_id
            /\ row.metric_project_id = response.resolved_project_id

CodeOutputWithinLimit ==
    req.tool = "code_on_fire" =>
        Cardinality(response.code_rows) <= response.effective_limit

DebtFiltersValidatedAndNormalized ==
    req.tool = "documented_tech_debt" /\ response.outcome = "ok" =>
        /\ response.format \in Formats
        /\ response.category \in Categories
        /\ (response.severity = "" \/ response.severity \in Severities)
        /\ response.format = FormatFor(req.format)
        /\ response.category = CategoryFor(req.category)
        /\ response.severity = SeverityFor(req.severity)

DebtMinAgeNonnegative ==
    req.tool = "documented_tech_debt" /\ response.outcome = "ok" =>
        req.min_age = NoMinAge \/ req.min_age >= 0

DebtLimitClamped ==
    req.tool = "documented_tech_debt" =>
        response.effective_limit = ClampDebtLimit(req.limit)

DebtFindingsProjectScoped ==
    req.tool = "documented_tech_debt" =>
        \A finding \in response.debt_findings :
            finding.project_id = response.resolved_project_id

DebtFindingsSatisfyFilters ==
    req.tool = "documented_tech_debt" /\ response.outcome = "ok" =>
        \A finding \in response.debt_findings :
            /\ CategoryMatches(finding, response.category)
            /\ (response.severity = "" \/ finding.severity = response.severity)
            /\ (response.kind = "" \/ finding.kind = response.kind)
            /\ (response.language = "" \/ finding.language = response.language)

DebtOutputWithinLimit ==
    req.tool = "documented_tech_debt" =>
        Cardinality(response.debt_findings) <= response.effective_limit

EnrichmentUsesResolvedProject ==
    response.outcome = "ok" =>
        response.effect_project_id = response.resolved_project_id

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        UniqueProjectRequired /\
        RejectedRequestsDoNotReturnRows /\
        CodeModeValidated /\
        CodeQuartilesValidated /\
        CodeLimitClamped /\
        CodeMetricRowsProjectConsistent /\
        CodeOutputWithinLimit /\
        DebtFiltersValidatedAndNormalized /\
        DebtMinAgeNonnegative /\
        DebtLimitClamped /\
        DebtFindingsProjectScoped /\
        DebtFindingsSatisfyFilters /\
        DebtOutputWithinLimit /\
        EnrichmentUsesResolvedProject)

=============================================================================
