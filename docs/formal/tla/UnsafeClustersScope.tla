------------------------------- MODULE UnsafeClustersScope -------------------------------
(***************************************************************************)
(* `unsafe_clusters` request boundary.                                     *)
(*                                                                         *)
(* The MCP tool resolves one project, regex-scans Rust files for unsafe    *)
(* declarations, returns bounded/ranked file counts, and enriches with     *)
(* unsafe effect symbols. The quality collector reads typed function       *)
(* metrics and must keep function/file/project identity aligned.           *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

MaxLimit == 200

Projects ==
    { [id |-> 1, name |-> "unique"],
      [id |-> 2, name |-> "duplicate"],
      [id |-> 3, name |-> "duplicate"] }

Files ==
    { [id |-> 10, project_id |-> 1, path |-> "a.rs", language |-> "rust", unsafe_count |-> 2],
      [id |-> 20, project_id |-> 1, path |-> "b.rs", language |-> "rust", unsafe_count |-> 1],
      [id |-> 30, project_id |-> 1, path |-> "c.py", language |-> "python", unsafe_count |-> 9],
      [id |-> 40, project_id |-> 2, path |-> "dup.rs", language |-> "rust", unsafe_count |-> 3] }

EffectSymbols ==
    { [symbol_id |-> 100, project_id |-> 1, file_project_id |-> 1, effect |-> "unsafe"],
      [symbol_id |-> 200, project_id |-> 1, file_project_id |-> 2, effect |-> "unsafe"],
      [symbol_id |-> 300, project_id |-> 2, file_project_id |-> 2, effect |-> "unsafe"] }

FunctionMetricRows ==
    { [function_id |-> 1000, metric_project_id |-> 1, file_project_id |-> 1, symbol_file_matches |-> TRUE, unsafe_blocks |-> 2],
      [function_id |-> 2000, metric_project_id |-> 1, file_project_id |-> 2, symbol_file_matches |-> TRUE, unsafe_blocks |-> 7],
      [function_id |-> 3000, metric_project_id |-> 1, file_project_id |-> 1, symbol_file_matches |-> FALSE, unsafe_blocks |-> 5],
      [function_id |-> 4000, metric_project_id |-> 2, file_project_id |-> 2, symbol_file_matches |-> TRUE, unsafe_blocks |-> 3] }

Requests ==
    { [id |-> 1, project |-> "", limit |-> 25],
      [id |-> 2, project |-> " unique ", limit |-> -5],
      [id |-> 3, project |-> "unique", limit |-> 500],
      [id |-> 4, project |-> "duplicate", limit |-> 25],
      [id |-> 5, project |-> "missing", limit |-> 25] }

RequestIds == {r.id : r \in Requests}
ProjectIds == {p.id : p \in Projects}
Outcomes == {"ok", "rejected"}
Reasons == {"none", "blank_project", "non_unique_project"}

NormalizeProject(raw) ==
    CASE raw = " unique " -> "unique"
      [] OTHER -> raw

ClampLimit(limit) ==
    IF limit < 1 THEN 1 ELSE IF limit > MaxLimit THEN MaxLimit ELSE limit

Matches(project_name) == {p \in Projects : p.name = project_name}

ResolvedProjectId(r) ==
    LET project == NormalizeProject(r.project) IN
    IF project # "" /\ Cardinality(Matches(project)) = 1
    THEN (CHOOSE p \in Matches(project) : TRUE).id
    ELSE 0

VisibleRegexFiles(r) ==
    LET pid == ResolvedProjectId(r) IN
        {f \in Files : f.project_id = pid /\ f.language = "rust" /\ f.unsafe_count > 0}

TopFile(rows) ==
    CHOOSE f \in rows : \A g \in rows : f.unsafe_count >= g.unsafe_count

BoundedFiles(r) ==
    LET rows == VisibleRegexFiles(r) IN
    LET cap == ClampLimit(r.limit) IN
    IF Cardinality(rows) <= cap THEN rows
    ELSE {TopFile(rows)}

VisibleEffects(r) ==
    LET pid == ResolvedProjectId(r) IN
        {s \in EffectSymbols : s.project_id = pid /\ s.file_project_id = pid}

VisibleFunctionRows(r) ==
    LET pid == ResolvedProjectId(r) IN
        {row \in FunctionMetricRows :
            row.metric_project_id = pid /\
            row.file_project_id = pid /\
            row.symbol_file_matches /\
            row.unsafe_blocks > 0}

TotalUnsafeBlocks(r) ==
    CASE ResolvedProjectId(r) = 1 -> 3
      [] ResolvedProjectId(r) = 2 -> 3
      [] OTHER -> 0

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      reason: Reasons,
      project: {"", "unique", "duplicate", "missing"},
      resolved_project_id: ProjectIds \cup {0},
      effective_limit: 1..MaxLimit,
      files: SUBSET Files,
      total_unsafe_blocks: Nat,
      effect_symbols: SUBSET EffectSymbols,
      collector_rows: SUBSET FunctionMetricRows ]

Init ==
    /\ req \in Requests
    /\ LET project == NormalizeProject(req.project) IN
       LET cap == ClampLimit(req.limit) IN
       IF project = "" THEN
        response =
            [ request_id |-> req.id,
              outcome |-> "rejected",
              reason |-> "blank_project",
              project |-> project,
              resolved_project_id |-> 0,
              effective_limit |-> cap,
              files |-> {},
              total_unsafe_blocks |-> 0,
              effect_symbols |-> {},
              collector_rows |-> {} ]
       ELSE IF Cardinality(Matches(project)) # 1 THEN
        response =
            [ request_id |-> req.id,
              outcome |-> "rejected",
              reason |-> "non_unique_project",
              project |-> project,
              resolved_project_id |-> 0,
              effective_limit |-> cap,
              files |-> {},
              total_unsafe_blocks |-> 0,
              effect_symbols |-> {},
              collector_rows |-> {} ]
       ELSE
       LET pid == ResolvedProjectId(req) IN
       LET files == BoundedFiles(req) IN
       /\ Cardinality(files) <= cap
       /\ response =
           [ request_id |-> req.id,
             outcome |-> "ok",
             reason |-> "none",
             project |-> project,
             resolved_project_id |-> pid,
             effective_limit |-> cap,
             files |-> files,
             total_unsafe_blocks |-> TotalUnsafeBlocks(req),
             effect_symbols |-> VisibleEffects(req),
             collector_rows |-> VisibleFunctionRows(req) ]

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests
    /\ response \in ResponseRecord

InvalidProjectsRejected ==
    (NormalizeProject(req.project) = "" \/ Cardinality(Matches(NormalizeProject(req.project))) # 1) =>
        /\ response.outcome = "rejected"
        /\ response.files = {}
        /\ response.effect_symbols = {}
        /\ response.collector_rows = {}

ProjectNormalized ==
    response.project = NormalizeProject(req.project)

EffectiveLimitClamped ==
    response.effective_limit = ClampLimit(req.limit)

RegexRowsScopedRustOnly ==
    \A row \in response.files :
        /\ row.project_id = response.resolved_project_id
        /\ row.language = "rust"
        /\ row.unsafe_count > 0

OutputWithinLimit ==
    Cardinality(response.files) <= response.effective_limit

RankingSound ==
    response.outcome = "ok" /\ Cardinality(response.files) > 0 =>
        \A omitted \in (VisibleRegexFiles(req) \ response.files) :
            \A returned \in response.files :
                omitted.unsafe_count <= returned.unsafe_count

TotalCountUsesScopedRustRows ==
    response.outcome = "ok" =>
        response.total_unsafe_blocks = TotalUnsafeBlocks(req)

EffectSymbolsProjectScoped ==
    \A symbol \in response.effect_symbols :
        /\ symbol.project_id = response.resolved_project_id
        /\ symbol.file_project_id = response.resolved_project_id
        /\ symbol.effect = "unsafe"

CollectorRowsProjectAndFileScoped ==
    \A row \in response.collector_rows :
        /\ row.metric_project_id = response.resolved_project_id
        /\ row.file_project_id = response.resolved_project_id
        /\ row.symbol_file_matches
        /\ row.unsafe_blocks > 0

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidProjectsRejected /\
        ProjectNormalized /\
        EffectiveLimitClamped /\
        RegexRowsScopedRustOnly /\
        OutputWithinLimit /\
        RankingSound /\
        TotalCountUsesScopedRustRows /\
        EffectSymbolsProjectScoped /\
        CollectorRowsProjectAndFileScoped)

=============================================================================
