--------------------------- MODULE DocCoverageGapsScope ---------------------------
(***************************************************************************)
(* `doc_coverage_gaps` request boundary.                                   *)
(*                                                                         *)
(* The production tool trims the requested project, rejects blank and       *)
(* duplicate display names, resolves at most one project id, and reuses     *)
(* that id for topic rows and effect-symbol enrichment.                     *)
(***************************************************************************)

EXTENDS Naturals, Sequences

ProjectIds == {"none", "p1", "p2", "duplicate"}
Projects == {"", " p ", "p", "missing", "dup"}
Statuses == {"well-documented", "under-documented", "undocumented"}

Requests ==
    { [id |-> 1, raw_project |-> ""],
      [id |-> 2, raw_project |-> "   "],
      [id |-> 3, raw_project |-> "dup"],
      [id |-> 4, raw_project |-> "missing"],
      [id |-> 5, raw_project |-> " p "],
      [id |-> 6, raw_project |-> "p"] }

RequestIds == {r.id : r \in Requests}

NormalizeProject(raw) ==
    CASE raw = " p " -> "p"
      [] raw = "   " -> ""
      [] OTHER -> raw

ResolveProject(project) ==
    CASE project = "" -> "none"
      [] project = "p" -> "p1"
      [] project = "missing" -> "none"
      [] project = "dup" -> "duplicate"
      [] OTHER -> "none"

TopicRowsFor(project_id) ==
    CASE project_id = "p1" ->
        << [topic_id |-> 1, project_id |-> "p1", label |-> "none", doc_chunks |-> 0, code_chunks |-> 20],
           [topic_id |-> 2, project_id |-> "p1", label |-> "under", doc_chunks |-> 2, code_chunks |-> 18],
           [topic_id |-> 3, project_id |-> "p1", label |-> "well", doc_chunks |-> 10, code_chunks |-> 5] >>
      [] project_id = "p2" ->
        << [topic_id |-> 4, project_id |-> "p2", label |-> "other", doc_chunks |-> 1, code_chunks |-> 0] >>
      [] OTHER -> <<>>

HasEffects(project_id) == project_id = "p1"

TopicStatus(row) ==
    LET total == row.doc_chunks + row.code_chunks IN
        CASE total = 0 -> "undocumented"
          [] row.doc_chunks * 100 > 30 * total -> "well-documented"
          [] row.doc_chunks * 100 > 5 * total -> "under-documented"
          [] OTHER -> "undocumented"

DecorateTopic(row) ==
    [ topic_id |-> row.topic_id,
      project_id |-> row.project_id,
      label |-> row.label,
      doc_chunks |-> row.doc_chunks,
      code_chunks |-> row.code_chunks,
      status |-> TopicStatus(row) ]

DecorateRows(rows) ==
    [i \in 1..Len(rows) |-> DecorateTopic(rows[i])]

ResponseRows ==
    [ topic_id: {1, 2, 3, 4},
      project_id: ProjectIds,
      label: {"none", "under", "well", "other"},
      doc_chunks: 0..10,
      code_chunks: 0..20,
      status: Statuses ]

ResponseFor(r) ==
    LET project == NormalizeProject(r.raw_project) IN
    LET project_id == ResolveProject(project) IN
        CASE project = "" ->
            [ request_id |-> r.id, project |-> "", project_id |-> "none",
              rejected |-> TRUE, reason |-> "blank", effects_project_id |-> "none",
              topics |-> <<>>, writes |-> 0, locks |-> 0 ]
          [] project_id = "duplicate" ->
            [ request_id |-> r.id, project |-> project, project_id |-> "none",
              rejected |-> TRUE, reason |-> "duplicate", effects_project_id |-> "none",
              topics |-> <<>>, writes |-> 0, locks |-> 0 ]
          [] OTHER ->
            [ request_id |-> r.id, project |-> project, project_id |-> project_id,
              rejected |-> FALSE, reason |-> "none",
              effects_project_id |-> IF HasEffects(project_id) THEN project_id ELSE "none",
              topics |-> DecorateRows(TopicRowsFor(project_id)),
              writes |-> 0, locks |-> 0 ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      project: {"", "p", "missing", "dup"},
      project_id: ProjectIds,
      rejected: BOOLEAN,
      reason: {"none", "blank", "duplicate"},
      effects_project_id: ProjectIds,
      topics: Seq(ResponseRows),
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

DuplicateProjectsRejected ==
    ResolveProject(NormalizeProject(req.raw_project)) = "duplicate" =>
        /\ response.rejected
        /\ response.reason = "duplicate"
        /\ Len(response.topics) = 0
        /\ response.effects_project_id = "none"

MissingProjectsHaveNoScopedData ==
    ~response.rejected /\ response.project_id = "none" =>
        /\ Len(response.topics) = 0
        /\ response.effects_project_id = "none"

TopicRowsProjectScoped ==
    ~response.rejected =>
        \A i \in 1..Len(response.topics) : response.topics[i].project_id = response.project_id

EffectsUseResolvedProject ==
    ~response.rejected =>
        (response.effects_project_id # "none" => response.effects_project_id = response.project_id)

StatusClassificationCorrect ==
    ~response.rejected =>
        \A i \in 1..Len(response.topics) :
            response.topics[i].status = TopicStatus(response.topics[i])

ProjectOutputNormalized ==
    response.project = NormalizeProject(req.raw_project)

ReadOnlyNoLocks ==
    /\ response.writes = 0
    /\ response.locks = 0

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        BlankProjectsRejected /\
        DuplicateProjectsRejected /\
        MissingProjectsHaveNoScopedData /\
        TopicRowsProjectScoped /\
        EffectsUseResolvedProject /\
        StatusClassificationCorrect /\
        ProjectOutputNormalized /\
        ReadOnlyNoLocks)

=============================================================================
