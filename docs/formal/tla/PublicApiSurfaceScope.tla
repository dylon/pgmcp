--------------------------- MODULE PublicApiSurfaceScope ---------------------------
(***************************************************************************)
(* `public_api_surface` request boundary.                                  *)
(*                                                                         *)
(* The production tool trims project/language, validates format, resolves a *)
(* unique project id, counts all public symbols for summary, and applies the *)
(* bounded row limit only to the full symbol list.                          *)
(***************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

ProjectIds == {"none", "p1", "p2", "duplicate"}
Formats == {"summary", "full", "xml"}
Languages == {"none", "rust", "python"}
Kinds == {"function", "struct"}

Requests ==
    { [id |-> 1, raw_project |-> "", raw_format |-> "summary", raw_language |-> "none", raw_limit |-> 500],
      [id |-> 2, raw_project |-> "dup", raw_format |-> "summary", raw_language |-> "none", raw_limit |-> 500],
      [id |-> 3, raw_project |-> " p ", raw_format |-> "summary", raw_language |-> " rust ", raw_limit |-> 1],
      [id |-> 4, raw_project |-> "p", raw_format |-> "full", raw_language |-> "none", raw_limit |-> 0],
      [id |-> 5, raw_project |-> "p", raw_format |-> "xml", raw_language |-> "none", raw_limit |-> 500] }

RequestIds == {r.id : r \in Requests}

NormalizeProject(raw) ==
    CASE raw = " p " -> "p"
      [] OTHER -> raw

NormalizeLanguage(raw) ==
    CASE raw = "none" -> "none"
      [] raw = " rust " -> "rust"
      [] OTHER -> raw

ResolveProject(project) ==
    CASE project = "" -> "none"
      [] project = "p" -> "p1"
      [] project = "dup" -> "duplicate"
      [] OTHER -> "none"

BoundLimit(raw) ==
    CASE raw < 1 -> 1
      [] raw > 2000 -> 2000
      [] OTHER -> raw

ApiRows(project_id) ==
    CASE project_id = "p1" ->
        << [project_id |-> "p1", language |-> "rust", kind |-> "function", symbol |-> "alpha"],
           [project_id |-> "p1", language |-> "rust", kind |-> "struct", symbol |-> "Beta"],
           [project_id |-> "p1", language |-> "python", kind |-> "function", symbol |-> "gamma"] >>
      [] OTHER -> <<>>

LanguageMatch(row, language) ==
    language = "none" \/ row.language = language

FilteredRows(project_id, language) ==
    SelectSeq(ApiRows(project_id), LAMBDA row: LanguageMatch(row, language))

Take(rows, n) ==
    SubSeq(rows, 1, IF Len(rows) < n THEN Len(rows) ELSE n)

KindCount(rows, kind) ==
    Cardinality({i \in 1..Len(rows) : rows[i].kind = kind})

ResponseFor(r) ==
    LET project == NormalizeProject(r.raw_project) IN
    LET format == r.raw_format IN
    LET language == NormalizeLanguage(r.raw_language) IN
    LET project_id == ResolveProject(project) IN
    LET limit == BoundLimit(r.raw_limit) IN
    LET rows == FilteredRows(project_id, language) IN
        CASE project = "" ->
            [ request_id |-> r.id, project |-> "", project_id |-> "none",
              format |-> format, language |-> language, rejected |-> TRUE,
              reason |-> "blank", limit |-> limit, total_public |-> 0,
              returned |-> 0, by_kind_total |-> 0, writes |-> 0, locks |-> 0 ]
          [] ~(format \in {"summary", "full"}) ->
            [ request_id |-> r.id, project |-> project, project_id |-> "none",
              format |-> format, language |-> language, rejected |-> TRUE,
              reason |-> "format", limit |-> limit, total_public |-> 0,
              returned |-> 0, by_kind_total |-> 0, writes |-> 0, locks |-> 0 ]
          [] project_id = "duplicate" ->
            [ request_id |-> r.id, project |-> project, project_id |-> "none",
              format |-> format, language |-> language, rejected |-> TRUE,
              reason |-> "duplicate", limit |-> limit, total_public |-> 0,
              returned |-> 0, by_kind_total |-> 0, writes |-> 0, locks |-> 0 ]
          [] OTHER ->
            [ request_id |-> r.id, project |-> project, project_id |-> project_id,
              format |-> format, language |-> language, rejected |-> FALSE,
              reason |-> "none", limit |-> limit, total_public |-> Len(rows),
              returned |-> IF format = "full" THEN Len(Take(rows, limit)) ELSE 0,
              by_kind_total |-> KindCount(rows, "function") + KindCount(rows, "struct"),
              writes |-> 0, locks |-> 0 ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      project: {"", "p", "dup"},
      project_id: ProjectIds,
      format: Formats,
      language: Languages,
      rejected: BOOLEAN,
      reason: {"none", "blank", "duplicate", "format"},
      limit: 1..2000,
      total_public: 0..3,
      returned: 0..3,
      by_kind_total: 0..3,
      writes: 0..0,
      locks: 0..0 ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK == response \in ResponseRecord

BlankProjectsRejected ==
    NormalizeProject(req.raw_project) = "" => response.rejected /\ response.reason = "blank"

InvalidFormatsRejected ==
    ~(req.raw_format \in {"summary", "full"}) => response.rejected /\ response.reason = "format"

DuplicateProjectsRejected ==
    ResolveProject(NormalizeProject(req.raw_project)) = "duplicate" /\ req.raw_format \in {"summary", "full"} =>
        /\ response.rejected
        /\ response.reason = "duplicate"

LimitBounded ==
    response.limit \in 1..2000

SummaryIgnoresFullLimit ==
    ~response.rejected /\ response.format = "summary" =>
        /\ response.total_public = response.by_kind_total
        /\ response.returned = 0

FullReturnedBounded ==
    ~response.rejected /\ response.format = "full" =>
        response.returned <= response.limit /\ response.returned <= response.total_public

LanguageFilterScoped ==
    ~response.rejected /\ response.language = "rust" =>
        response.total_public = 2

ProjectOutputNormalized ==
    ~response.rejected /\ req.raw_project = " p " => response.project = "p"

ReadOnlyNoLocks ==
    /\ response.writes = 0
    /\ response.locks = 0

=============================================================================
