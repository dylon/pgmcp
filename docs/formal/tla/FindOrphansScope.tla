---------------------------- MODULE FindOrphansScope ----------------------------
(***************************************************************************)
(* `find_orphans` request boundary.                                       *)
(*                                                                         *)
(* The production tool validates detail, rejects blank supplied projects,   *)
(* resolves supplied project names uniquely, clamps limits, scopes rows by  *)
(* resolved project id and language, and stays read-only.                  *)
(***************************************************************************)

EXTENDS Naturals, FiniteSets

ProjectIds == {"none", "p1", "p2", "duplicate"}
Details == {"files", "chunks", "summary"}
Languages == {"none", "rust", "python"}
Rows == {"p1-rust", "p1-python", "p2-rust"}

Requests ==
    { [id |-> 1, raw_project |-> "none", raw_language |-> "none", raw_detail |-> "files", raw_limit |-> 50],
      [id |-> 2, raw_project |-> "   ", raw_language |-> "none", raw_detail |-> "files", raw_limit |-> 50],
      [id |-> 3, raw_project |-> "dup", raw_language |-> "none", raw_detail |-> "chunks", raw_limit |-> 50],
      [id |-> 4, raw_project |-> " p ", raw_language |-> " rust ", raw_detail |-> " chunks ", raw_limit |-> 0],
      [id |-> 5, raw_project |-> "p", raw_language |-> "none", raw_detail |-> "summary", raw_limit |-> 50] }

RequestIds == {r.id : r \in Requests}

NormalizeProject(raw) ==
    CASE raw = "none" -> "none"
      [] raw = " p " -> "p"
      [] raw = "   " -> ""
      [] OTHER -> raw

NormalizeLanguage(raw) ==
    CASE raw = "none" -> "none"
      [] raw = " rust " -> "rust"
      [] OTHER -> raw

NormalizeDetail(raw) ==
    CASE raw = " chunks " -> "chunks"
      [] OTHER -> raw

ResolveProject(project) ==
    CASE project = "none" -> "none"
      [] project = "" -> "none"
      [] project = "p" -> "p1"
      [] project = "dup" -> "duplicate"
      [] OTHER -> "none"

BoundLimit(raw) ==
    CASE raw < 1 -> 1
      [] raw > 1000 -> 1000
      [] OTHER -> raw

OrphanRows ==
    { [id |-> "p1-rust", project_id |-> "p1", language |-> "rust"],
      [id |-> "p1-python", project_id |-> "p1", language |-> "python"],
      [id |-> "p2-rust", project_id |-> "p2", language |-> "rust"] }

RowMatches(row, project_id, language) ==
    /\ (project_id = "none" \/ row.project_id = project_id)
    /\ (language = "none" \/ row.language = language)

ScopedRows(project_id, language) ==
    {row.id : row \in {candidate \in OrphanRows : RowMatches(candidate, project_id, language)}}

Min(a, b) == IF a < b THEN a ELSE b

ResponseFor(r) ==
    LET project == NormalizeProject(r.raw_project) IN
    LET language == NormalizeLanguage(r.raw_language) IN
    LET detail == NormalizeDetail(r.raw_detail) IN
    LET project_id == ResolveProject(project) IN
    LET limit == BoundLimit(r.raw_limit) IN
    LET rows == ScopedRows(project_id, language) IN
        CASE project = "" ->
            [ request_id |-> r.id,
              project |-> project,
              project_id |-> "none",
              language |-> language,
              detail |-> detail,
              rejected |-> TRUE,
              reason |-> "blank_project",
              limit |-> limit,
              rows |-> {},
              returned |-> 0,
              enrichment_project_id |-> "none",
              writes |-> 0,
              locks |-> 0 ]
          [] ~(detail \in {"files", "chunks"}) ->
            [ request_id |-> r.id,
              project |-> project,
              project_id |-> "none",
              language |-> language,
              detail |-> detail,
              rejected |-> TRUE,
              reason |-> "detail",
              limit |-> limit,
              rows |-> {},
              returned |-> 0,
              enrichment_project_id |-> "none",
              writes |-> 0,
              locks |-> 0 ]
          [] project_id = "duplicate" ->
            [ request_id |-> r.id,
              project |-> project,
              project_id |-> "none",
              language |-> language,
              detail |-> detail,
              rejected |-> TRUE,
              reason |-> "duplicate",
              limit |-> limit,
              rows |-> {},
              returned |-> 0,
              enrichment_project_id |-> "none",
              writes |-> 0,
              locks |-> 0 ]
          [] OTHER ->
            [ request_id |-> r.id,
              project |-> project,
              project_id |-> project_id,
              language |-> language,
              detail |-> detail,
              rejected |-> FALSE,
              reason |-> "none",
              limit |-> limit,
              rows |-> rows,
              returned |-> Min(Cardinality(rows), limit),
              enrichment_project_id |-> project_id,
              writes |-> 0,
              locks |-> 0 ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      project: {"none", "", "p", "dup"},
      project_id: ProjectIds,
      language: Languages,
      detail: Details,
      rejected: BOOLEAN,
      reason: {"none", "blank_project", "duplicate", "detail"},
      limit: 1..1000,
      rows: SUBSET Rows,
      returned: 0..3,
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

BlankSuppliedProjectRejected ==
    NormalizeProject(req.raw_project) = "" => response.rejected /\ response.reason = "blank_project"

InvalidDetailRejected ==
    ~(NormalizeDetail(req.raw_detail) \in {"files", "chunks"}) =>
        /\ response.rejected
        /\ response.reason = "detail"

DuplicateProjectsRejected ==
    ResolveProject(NormalizeProject(req.raw_project)) = "duplicate"
    /\ NormalizeDetail(req.raw_detail) \in {"files", "chunks"} =>
        /\ response.rejected
        /\ response.reason = "duplicate"

LimitBounded ==
    response.limit \in 1..1000

ProjectOutputNormalized ==
    ~response.rejected /\ req.raw_project = " p " => response.project = "p"

DetailOutputNormalized ==
    ~response.rejected => response.detail \in {"files", "chunks"}

ScopedRowsMatchFilters ==
    ~response.rejected =>
        /\ response.rows = ScopedRows(response.project_id, response.language)
        /\ response.returned <= response.limit

LanguageFilterSound ==
    ~response.rejected /\ response.language = "rust" =>
        "p1-python" \notin response.rows

EffectEnrichmentUsesResolvedProject ==
    ~response.rejected /\ response.project_id # "none" =>
        response.enrichment_project_id = response.project_id

ReadOnlyNoLocks ==
    /\ response.writes = 0
    /\ response.locks = 0

=============================================================================
