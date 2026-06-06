------------------------ MODULE EngineeringScorecardScope ------------------------
(***************************************************************************)
(* `engineering_scorecard` and the quality-history cron boundary.           *)
(*                                                                         *)
(* The model covers normalized public requests, duplicate-name fail-closed   *)
(* behavior, id-scoped god-file and import-cycle metrics, and the cron path  *)
(* that snapshots already-listed project ids without re-resolving names.    *)
(***************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

ProjectIds == {"none", "p1", "p2", "duplicate"}
Modes == {"tool", "cron"}

Requests ==
    { [id |-> 1, mode |-> "tool", raw_project |-> "", project_id |-> "none"],
      [id |-> 2, mode |-> "tool", raw_project |-> "   ", project_id |-> "none"],
      [id |-> 3, mode |-> "tool", raw_project |-> "dup", project_id |-> "none"],
      [id |-> 4, mode |-> "tool", raw_project |-> " p ", project_id |-> "none"],
      [id |-> 5, mode |-> "cron", raw_project |-> "dup", project_id |-> "p1"],
      [id |-> 6, mode |-> "cron", raw_project |-> "dup", project_id |-> "p2"] }

RequestIds == {r.id : r \in Requests}

NormalizeProject(raw) ==
    CASE raw = " p " -> "p"
      [] raw = "   " -> ""
      [] OTHER -> raw

ResolveProject(project) ==
    CASE project = "" -> "none"
      [] project = "p" -> "p1"
      [] project = "dup" -> "duplicate"
      [] OTHER -> "none"

RequestProjectId(r) ==
    IF r.mode = "cron" THEN r.project_id ELSE ResolveProject(NormalizeProject(r.raw_project))

CycleFiles(project_id) ==
    CASE project_id = "p1" -> {"a.rs", "b.rs", "c.rs"}
      [] OTHER -> {}

GodFiles(project_id) ==
    CASE project_id = "p2" -> {"huge.rs"}
      [] OTHER -> {}

ResponseFor(r) ==
    LET project == NormalizeProject(r.raw_project) IN
    LET project_id == RequestProjectId(r) IN
        CASE project = "" ->
            [ request_id |-> r.id,
              mode |-> r.mode,
              project |-> "",
              project_id |-> "none",
              rejected |-> TRUE,
              reason |-> "blank",
              cycle_file_count |-> 0,
              god_file_count |-> 0,
              no_circular_deps |-> TRUE,
              no_god_files |-> TRUE,
              snapshot_written |-> FALSE,
              writes |-> 0,
              locks |-> 0 ]
          [] r.mode = "tool" /\ project_id = "duplicate" ->
            [ request_id |-> r.id,
              mode |-> r.mode,
              project |-> project,
              project_id |-> "none",
              rejected |-> TRUE,
              reason |-> "duplicate",
              cycle_file_count |-> 0,
              god_file_count |-> 0,
              no_circular_deps |-> TRUE,
              no_god_files |-> TRUE,
              snapshot_written |-> FALSE,
              writes |-> 0,
              locks |-> 0 ]
          [] OTHER ->
            [ request_id |-> r.id,
              mode |-> r.mode,
              project |-> project,
              project_id |-> project_id,
              rejected |-> FALSE,
              reason |-> "none",
              cycle_file_count |-> Cardinality(CycleFiles(project_id)),
              god_file_count |-> Cardinality(GodFiles(project_id)),
              no_circular_deps |-> Cardinality(CycleFiles(project_id)) = 0,
              no_god_files |-> Cardinality(GodFiles(project_id)) = 0,
              snapshot_written |-> r.mode = "cron",
              writes |-> IF r.mode = "cron" THEN 1 ELSE 0,
              locks |-> 0 ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      mode: Modes,
      project: {"", "p", "dup"},
      project_id: ProjectIds,
      rejected: BOOLEAN,
      reason: {"none", "blank", "duplicate"},
      cycle_file_count: 0..3,
      god_file_count: 0..1,
      no_circular_deps: BOOLEAN,
      no_god_files: BOOLEAN,
      snapshot_written: BOOLEAN,
      writes: 0..1,
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

PublicDuplicateNamesRejected ==
    req.mode = "tool" /\ ResolveProject(NormalizeProject(req.raw_project)) = "duplicate" =>
        /\ response.rejected
        /\ response.reason = "duplicate"
        /\ response.project_id = "none"

CronDuplicateNamesUseListedIds ==
    req.mode = "cron" =>
        /\ ~response.rejected
        /\ response.project_id = req.project_id
        /\ response.snapshot_written

CycleMetricProjectScoped ==
    ~response.rejected =>
        response.cycle_file_count = Cardinality(CycleFiles(response.project_id))

GodFileMetricProjectScoped ==
    ~response.rejected =>
        response.god_file_count = Cardinality(GodFiles(response.project_id))

TransitiveImportCycleDetected ==
    response.project_id = "p1" =>
        /\ response.cycle_file_count = 3
        /\ ~response.no_circular_deps

GodFileGateUsesResolvedProject ==
    response.project_id = "p2" =>
        /\ response.god_file_count = 1
        /\ ~response.no_god_files

ProjectOutputNormalized ==
    ~response.rejected /\ req.mode = "tool" /\ req.raw_project = " p " =>
        response.project = "p"

NoRuntimeLocks ==
    response.locks = 0

ToolPathReadOnly ==
    response.mode = "tool" => response.writes = 0

=============================================================================
