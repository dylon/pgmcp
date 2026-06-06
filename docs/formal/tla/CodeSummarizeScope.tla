------------------------------- MODULE CodeSummarizeScope -------------------------------
(***************************************************************************)
(* `code_summarize` request boundary.                                      *)
(*                                                                         *)
(* The tool resolves one project id, validates scope/detail/path inputs,    *)
(* applies one literal path scope to directory, key-file, and language      *)
(* channels, and scopes topic/effect enrichment to the same project.        *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

Scopes == {"project", "directory", "file"}
Details == {"brief", "standard", "detailed"}
Outcomes == {"ok", "rejected"}
Reasons == {"none", "blank_project", "project_resolution", "invalid_scope", "invalid_detail", "path_required"}

Projects ==
    { [id |-> 1, name |-> "unique"],
      [id |-> 2, name |-> "duplicate"],
      [id |-> 3, name |-> "duplicate"] }

ProjectIds == {p.id : p \in Projects}
ProjectNames == {p.name : p \in Projects} \cup {"missing", ""}

Files ==
    { [id |-> 10, project_id |-> 1, path |-> "src/a.rs", language |-> "rust"],
      [id |-> 11, project_id |-> 1, path |-> "tests/b.rs", language |-> "rust"],
      [id |-> 12, project_id |-> 1, path |-> "literal_%/c.rs", language |-> "rust"],
      [id |-> 20, project_id |-> 2, path |-> "src/dup.rs", language |-> "rust"],
      [id |-> 30, project_id |-> 3, path |-> "src/other.rs", language |-> "rust"] }

MetricRows ==
    { [file_id |-> 10, file_project_id |-> 1, metric_project_id |-> 1],
      [file_id |-> 11, file_project_id |-> 1, metric_project_id |-> 2],
      [file_id |-> 20, file_project_id |-> 2, metric_project_id |-> 2],
      [file_id |-> 30, file_project_id |-> 3, metric_project_id |-> 3] }

Topics ==
    { [id |-> 1, projects |-> {"unique"}, label |-> "unique-topic"],
      [id |-> 2, projects |-> {"duplicate"}, label |-> "duplicate-topic"],
      [id |-> 3, projects |-> {"other"}, label |-> "other-topic"] }

Effects ==
    { [project_id |-> 1, effect |-> "unsafe"],
      [project_id |-> 2, effect |-> "may_panic"],
      [project_id |-> 3, effect |-> "blocking_io"] }

Requests ==
    { [id |-> 1, project |-> " unique ", scope |-> "", path |-> "", detail |-> ""],
      [id |-> 2, project |-> "unique", scope |-> " directory ", path |-> " src/ ", detail |-> " brief "],
      [id |-> 3, project |-> "unique", scope |-> "file", path |-> "tests/b.rs", detail |-> "detailed"],
      [id |-> 4, project |-> "duplicate", scope |-> "", path |-> "", detail |-> ""],
      [id |-> 5, project |-> "unique", scope |-> "workspace", path |-> "", detail |-> ""],
      [id |-> 6, project |-> "unique", scope |-> "file", path |-> "", detail |-> ""],
      [id |-> 7, project |-> "unique", scope |-> "", path |-> "", detail |-> "verbose"],
      [id |-> 8, project |-> "unique", scope |-> "directory", path |-> "literal_%/", detail |-> "standard"] }

RequestIds == {r.id : r \in Requests}

Trim(s) ==
    CASE s = " unique " -> "unique"
      [] s = " directory " -> "directory"
      [] s = " src/ " -> "src/"
      [] s = " brief " -> "brief"
      [] OTHER -> s

ScopeFor(raw) ==
    LET s == Trim(raw) IN IF s = "" THEN "project" ELSE s

DetailFor(raw) ==
    LET d == Trim(raw) IN IF d = "" THEN "standard" ELSE d

PathFor(raw) == Trim(raw)

Matches(project_name) == {p \in Projects : p.name = Trim(project_name)}

ResolvedProjectId(r) ==
    IF Cardinality(Matches(r.project)) = 1
    THEN (CHOOSE p \in Matches(r.project) : TRUE).id
    ELSE 0

PrefixMatches(path, prefix) ==
    CASE prefix = "src/" -> path = "src/a.rs"
      [] prefix = "literal_%/" -> path = "literal_%/c.rs"
      [] OTHER -> FALSE

PathMatches(r, file) ==
    LET scope == ScopeFor(r.scope) IN
    LET path == PathFor(r.path) IN
        CASE scope = "project" -> TRUE
          [] scope = "directory" -> PrefixMatches(file.path, path)
          [] scope = "file" -> file.path = path
          [] OTHER -> FALSE

VisibleFiles(r) ==
    {file \in Files :
        /\ file.project_id = ResolvedProjectId(r)
        /\ PathMatches(r, file)}

MetricVisible(r, metric) ==
    \E file \in VisibleFiles(r) :
        /\ file.id = metric.file_id
        /\ metric.file_project_id = file.project_id
        /\ metric.metric_project_id = file.project_id

VisibleMetrics(r) == {metric \in MetricRows : MetricVisible(r, metric)}

VisibleTopics(r) ==
    {topic \in Topics : Trim(r.project) \in topic.projects}

VisibleEffects(r) ==
    {effect \in Effects : effect.project_id = ResolvedProjectId(r)}

ReasonFor(r) ==
    LET project == Trim(r.project) IN
    LET scope == ScopeFor(r.scope) IN
    LET detail == DetailFor(r.detail) IN
    LET path == PathFor(r.path) IN
        CASE project = "" -> "blank_project"
          [] Cardinality(Matches(r.project)) # 1 -> "project_resolution"
          [] ~(scope \in Scopes) -> "invalid_scope"
          [] ~(detail \in Details) -> "invalid_detail"
          [] scope \in {"directory", "file"} /\ path = "" -> "path_required"
          [] OTHER -> "none"

ResponseFor(r) ==
    LET reason == ReasonFor(r) IN
    LET ok == reason = "none" IN
        [ request_id |-> r.id,
          outcome |-> IF ok THEN "ok" ELSE "rejected",
          reason |-> reason,
          project |-> Trim(r.project),
          resolved_project_id |-> IF ok THEN ResolvedProjectId(r) ELSE 0,
          scope |-> ScopeFor(r.scope),
          detail |-> DetailFor(r.detail),
          path |-> PathFor(r.path),
          files |-> IF ok THEN VisibleFiles(r) ELSE {},
          key_metric_rows |-> IF ok THEN VisibleMetrics(r) ELSE {},
          language_files |-> IF ok THEN VisibleFiles(r) ELSE {},
          topics_included |-> ok /\ DetailFor(r.detail) # "brief",
          topics |-> IF ok /\ DetailFor(r.detail) # "brief" THEN VisibleTopics(r) ELSE {},
          effects |-> IF ok THEN VisibleEffects(r) ELSE {} ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      reason: Reasons,
      project: ProjectNames,
      resolved_project_id: ProjectIds \cup {0},
      scope: Scopes \cup {"workspace"},
      detail: Details \cup {"verbose"},
      path: {"", "src/", "tests/b.rs", "literal_%/"},
      files: SUBSET Files,
      key_metric_rows: SUBSET MetricRows,
      language_files: SUBSET Files,
      topics_included: BOOLEAN,
      topics: SUBSET Topics,
      effects: SUBSET Effects ]

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
        /\ response.files = {}
        /\ response.key_metric_rows = {}
        /\ response.language_files = {}
        /\ response.topics = {}
        /\ response.effects = {}

ScopeAndDetailValidated ==
    response.outcome = "ok" =>
        /\ response.scope \in Scopes
        /\ response.detail \in Details

PathRequiredForSubprojectScope ==
    ScopeFor(req.scope) \in {"directory", "file"} /\ PathFor(req.path) = "" =>
        response.reason = "path_required"

AllFileChannelsUseSameScope ==
    response.outcome = "ok" =>
        /\ response.files = VisibleFiles(req)
        /\ response.language_files = VisibleFiles(req)
        /\ \A metric \in response.key_metric_rows :
            \E file \in VisibleFiles(req) : file.id = metric.file_id

ReturnedFilesProjectScoped ==
    \A file \in response.files \cup response.language_files :
        file.project_id = response.resolved_project_id

MetricRowsProjectConsistent ==
    \A metric \in response.key_metric_rows :
        /\ metric.file_project_id = response.resolved_project_id
        /\ metric.metric_project_id = response.resolved_project_id

TopicsRespectDetailAndProject ==
    /\ (response.detail = "brief" => response.topics = {})
    /\ \A topic \in response.topics : response.project \in topic.projects

EffectsProjectScoped ==
    \A effect \in response.effects :
        effect.project_id = response.resolved_project_id

LiteralDirectoryPathMatching ==
    response.outcome = "ok" /\ response.scope = "directory" /\ response.path = "literal_%/" =>
        \A file \in response.files : file.path = "literal_%/c.rs"

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        UniqueProjectRequired /\
        ScopeAndDetailValidated /\
        PathRequiredForSubprojectScope /\
        AllFileChannelsUseSameScope /\
        ReturnedFilesProjectScoped /\
        MetricRowsProjectConsistent /\
        TopicsRespectDetailAndProject /\
        EffectsProjectScoped /\
        LiteralDirectoryPathMatching)

=============================================================================
