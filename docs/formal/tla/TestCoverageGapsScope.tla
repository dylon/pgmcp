--------------------------- MODULE TestCoverageGapsScope ---------------------------
(***************************************************************************)
(* `test_coverage_gaps` request boundary.                                  *)
(*                                                                         *)
(* The production tool resolves a project display name to at most one       *)
(* project id, then reuses that id for topic coverage, real coverage        *)
(* artifacts, and effect-symbol enrichment.                                 *)
(*                                                                         *)
(* Verified:                                                               *)
(*   - empty project names are rejected before query;                       *)
(*   - duplicate project display names fail closed;                         *)
(*   - missing projects do not return scoped rows or enrichment;            *)
(*   - topic rows, coverage artifacts, and effect counts use the same       *)
(*     resolved project id;                                                 *)
(*   - per-topic status matches the integer threshold table;                *)
(*   - telemetry/output project text is the trimmed request project.        *)
(***************************************************************************)

EXTENDS Naturals, Sequences

ProjectIds == {"none", "p1", "p2", "duplicate"}
Projects == {"", " p ", "p", "missing", "dup"}
CoverageSources == {"topic_proxy", "report+topic_proxy"}
Statuses == {"well-tested", "under-tested", "untested"}

NoReq == [id |-> 0, raw_project |-> ""]

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
        << [topic_id |-> 1, project_id |-> "p1", label |-> "none", test_chunks |-> 0, impl_chunks |-> 20],
           [topic_id |-> 2, project_id |-> "p1", label |-> "under", test_chunks |-> 2, impl_chunks |-> 18],
           [topic_id |-> 3, project_id |-> "p1", label |-> "well", test_chunks |-> 10, impl_chunks |-> 5] >>
      [] project_id = "p2" ->
        << [topic_id |-> 4, project_id |-> "p2", label |-> "other", test_chunks |-> 1, impl_chunks |-> 0] >>
      [] OTHER -> <<>>

HasRealCoverage(project_id) == project_id = "p1"
HasEffects(project_id) == project_id = "p1"

CoverageSource(project_id) ==
    IF HasRealCoverage(project_id) THEN "report+topic_proxy" ELSE "topic_proxy"

TopicStatus(row) ==
    LET total == row.test_chunks + row.impl_chunks IN
        CASE total = 0 -> "untested"
          [] row.test_chunks * 100 > 30 * total -> "well-tested"
          [] row.test_chunks * 100 > 1 * total -> "under-tested"
          [] OTHER -> "untested"

DecorateTopic(row) ==
    [ topic_id |-> row.topic_id,
      project_id |-> row.project_id,
      label |-> row.label,
      test_chunks |-> row.test_chunks,
      impl_chunks |-> row.impl_chunks,
      status |-> TopicStatus(row) ]

DecorateRows(rows) ==
    [i \in 1..Len(rows) |-> DecorateTopic(rows[i])]

ResponseRows ==
    [ topic_id: {1, 2, 3, 4},
      project_id: ProjectIds,
      label: {"none", "under", "well", "other"},
      test_chunks: 0..10,
      impl_chunks: 0..20,
      status: Statuses ]

NoResp ==
    [ project |-> "",
      project_id |-> "none",
      rejected |-> FALSE,
      reason |-> "none",
      coverage_source |-> "topic_proxy",
      real_coverage_project_id |-> "none",
      effects_project_id |-> "none",
      topics |-> <<>>,
      telemetry_project |-> "" ]

VARIABLES req, status, resp

vars == <<req, status, resp>>

Init ==
    /\ req = NoReq
    /\ status = "idle"
    /\ resp = NoResp

PickRequest(r) ==
    /\ status = "idle"
    /\ r \in Requests
    /\ req' = r
    /\ status' = "pending"
    /\ UNCHANGED resp

RejectBlank ==
    /\ status = "pending"
    /\ NormalizeProject(req.raw_project) = ""
    /\ resp' = [NoResp EXCEPT
        !.project = "",
        !.rejected = TRUE,
        !.reason = "blank",
        !.telemetry_project = ""]
    /\ status' = "done"
    /\ UNCHANGED req

RejectDuplicate ==
    /\ status = "pending"
    /\ NormalizeProject(req.raw_project) # ""
    /\ ResolveProject(NormalizeProject(req.raw_project)) = "duplicate"
    /\ resp' = [NoResp EXCEPT
        !.project = NormalizeProject(req.raw_project),
        !.rejected = TRUE,
        !.reason = "duplicate",
        !.telemetry_project = NormalizeProject(req.raw_project)]
    /\ status' = "done"
    /\ UNCHANGED req

Respond ==
    /\ status = "pending"
    /\ LET project == NormalizeProject(req.raw_project) IN
       LET project_id == ResolveProject(project) IN
       /\ project # ""
       /\ project_id # "duplicate"
       /\ resp' =
            [ project |-> project,
              project_id |-> project_id,
              rejected |-> FALSE,
              reason |-> "none",
              coverage_source |-> CoverageSource(project_id),
              real_coverage_project_id |-> IF HasRealCoverage(project_id) THEN project_id ELSE "none",
              effects_project_id |-> IF HasEffects(project_id) THEN project_id ELSE "none",
              topics |-> DecorateRows(TopicRowsFor(project_id)),
              telemetry_project |-> project ]
    /\ status' = "done"
    /\ UNCHANGED req

TerminalStutter ==
    /\ status = "done"
    /\ UNCHANGED vars

Next ==
    \/ \E r \in Requests : PickRequest(r)
    \/ RejectBlank
    \/ RejectDuplicate
    \/ Respond
    \/ TerminalStutter

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ status \in {"idle", "pending", "done"}
    /\ resp.project \in Projects
    /\ resp.project_id \in ProjectIds
    /\ resp.rejected \in BOOLEAN
    /\ resp.reason \in {"none", "blank", "duplicate"}
    /\ resp.coverage_source \in CoverageSources
    /\ resp.real_coverage_project_id \in ProjectIds
    /\ resp.effects_project_id \in ProjectIds
    /\ resp.topics \in Seq(ResponseRows)
    /\ resp.telemetry_project \in Projects

BlankProjectsRejected ==
    status = "done" /\ NormalizeProject(req.raw_project) = "" => resp.rejected /\ resp.reason = "blank"

DuplicateProjectsRejected ==
    status = "done" /\ ResolveProject(NormalizeProject(req.raw_project)) = "duplicate" =>
        /\ resp.rejected
        /\ resp.reason = "duplicate"
        /\ Len(resp.topics) = 0
        /\ resp.real_coverage_project_id = "none"
        /\ resp.effects_project_id = "none"

MissingProjectsHaveNoScopedData ==
    status = "done" /\ ~resp.rejected /\ resp.project_id = "none" =>
        /\ Len(resp.topics) = 0
        /\ resp.real_coverage_project_id = "none"
        /\ resp.effects_project_id = "none"

TopicRowsProjectScoped ==
    status = "done" /\ ~resp.rejected =>
        \A i \in 1..Len(resp.topics) : resp.topics[i].project_id = resp.project_id

CoverageAndEffectsUseResolvedProject ==
    status = "done" /\ ~resp.rejected =>
        /\ (resp.real_coverage_project_id # "none" => resp.real_coverage_project_id = resp.project_id)
        /\ (resp.effects_project_id # "none" => resp.effects_project_id = resp.project_id)

StatusClassificationCorrect ==
    status = "done" /\ ~resp.rejected =>
        \A i \in 1..Len(resp.topics) :
            resp.topics[i].status = TopicStatus(resp.topics[i])

TelemetryProjectNormalized ==
    status = "done" => resp.telemetry_project = NormalizeProject(req.raw_project)

CoverageSourceMatchesRealCoverage ==
    status = "done" /\ ~resp.rejected =>
        resp.coverage_source =
            IF resp.real_coverage_project_id # "none" THEN "report+topic_proxy" ELSE "topic_proxy"

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        BlankProjectsRejected /\
        DuplicateProjectsRejected /\
        MissingProjectsHaveNoScopedData /\
        TopicRowsProjectScoped /\
        CoverageAndEffectsUseResolvedProject /\
        StatusClassificationCorrect /\
        TelemetryProjectNormalized /\
        CoverageSourceMatchesRealCoverage)

=============================================================================
