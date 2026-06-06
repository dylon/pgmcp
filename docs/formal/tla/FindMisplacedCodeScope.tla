-------------------------- MODULE FindMisplacedCodeScope --------------------------
(***************************************************************************)
(* `find_misplaced_code` request boundary.                                 *)
(*                                                                         *)
(* The production tool trims the project, bounds min_mismatch, resolves a   *)
(* unique project id, loads file/topic rows by that id, and reuses the same *)
(* id for effect enrichment.                                                *)
(***************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

ProjectIds == {"none", "p1", "p2", "duplicate"}
Paths == {"auth/a.rs", "auth/b.rs", "auth/log.rs", "other/x.rs"}
Topics == {"auth", "log", "db"}

Requests ==
    { [id |-> 1, raw_project |-> "", raw_min |-> 5],
      [id |-> 2, raw_project |-> "   ", raw_min |-> 5],
      [id |-> 3, raw_project |-> "dup", raw_min |-> 5],
      [id |-> 4, raw_project |-> " p ", raw_min |-> 0 - 5],
      [id |-> 5, raw_project |-> "p", raw_min |-> 15] }

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

BoundMin(raw) ==
    CASE raw < 0 -> 0
      [] raw > 10 -> 10
      [] OTHER -> raw

TopicRows(project_id) ==
    CASE project_id = "p1" ->
        << [path |-> "auth/a.rs", project_id |-> "p1", dir |-> "auth", topic |-> "auth"],
           [path |-> "auth/b.rs", project_id |-> "p1", dir |-> "auth", topic |-> "auth"],
           [path |-> "auth/log.rs", project_id |-> "p1", dir |-> "auth", topic |-> "log"],
           [path |-> "other/x.rs", project_id |-> "p1", dir |-> "other", topic |-> "db"] >>
      [] project_id = "p2" ->
        << [path |-> "auth/log.rs", project_id |-> "p2", dir |-> "auth", topic |-> "log"] >>
      [] OTHER -> <<>>

DirSize(rows, dir) ==
    Cardinality({i \in 1..Len(rows) : rows[i].dir = dir})

TopicCount(rows, dir, topic) ==
    Cardinality({i \in 1..Len(rows) : rows[i].dir = dir /\ rows[i].topic = topic})

MajorityTopic(rows, dir) ==
    IF TopicCount(rows, dir, "auth") >= TopicCount(rows, dir, "log") /\
       TopicCount(rows, dir, "auth") >= TopicCount(rows, dir, "db")
    THEN "auth"
    ELSE IF TopicCount(rows, dir, "log") >= TopicCount(rows, dir, "db")
    THEN "log"
    ELSE "db"

MismatchTimes10(rows, row) ==
    LET n == DirSize(rows, row.dir) IN
        IF n = 0 THEN 0 ELSE 10 - ((10 * TopicCount(rows, row.dir, row.topic)) \div n)

MisplacedRows(rows, min10) ==
    [i \in 1..Len(rows) |->
        IF /\ DirSize(rows, rows[i].dir) > 1
           /\ rows[i].topic # MajorityTopic(rows, rows[i].dir)
           /\ MismatchTimes10(rows, rows[i]) >= min10
        THEN [path |-> rows[i].path, project_id |-> rows[i].project_id,
              mismatch |-> MismatchTimes10(rows, rows[i])]
        ELSE [path |-> "other/x.rs", project_id |-> "none", mismatch |-> 0]]

Kept(rows) ==
    SelectSeq(rows, LAMBDA r: r.project_id # "none")

HasEffects(project_id) == project_id = "p1"

ResponseFor(r) ==
    LET project == NormalizeProject(r.raw_project) IN
    LET project_id == ResolveProject(project) IN
    LET min10 == BoundMin(r.raw_min) IN
    LET rows == TopicRows(project_id) IN
        CASE project = "" ->
            [ request_id |-> r.id, project |-> "", project_id |-> "none",
              rejected |-> TRUE, reason |-> "blank", min10 |-> min10,
              rows |-> <<>>, effects_project_id |-> "none", writes |-> 0, locks |-> 0 ]
          [] project_id = "duplicate" ->
            [ request_id |-> r.id, project |-> project, project_id |-> "none",
              rejected |-> TRUE, reason |-> "duplicate", min10 |-> min10,
              rows |-> <<>>, effects_project_id |-> "none", writes |-> 0, locks |-> 0 ]
          [] OTHER ->
            [ request_id |-> r.id, project |-> project, project_id |-> project_id,
              rejected |-> FALSE, reason |-> "none", min10 |-> min10,
              rows |-> Kept(MisplacedRows(rows, min10)),
              effects_project_id |-> IF HasEffects(project_id) THEN project_id ELSE "none",
              writes |-> 0, locks |-> 0 ]

VARIABLES req, response

vars == <<req, response>>

MisplacedRecord == [path: Paths, project_id: ProjectIds, mismatch: 0..10]

ResponseRecord ==
    [ request_id: RequestIds,
      project: {"", "p", "dup"},
      project_id: ProjectIds,
      rejected: BOOLEAN,
      reason: {"none", "blank", "duplicate"},
      min10: 0..10,
      rows: Seq(MisplacedRecord),
      effects_project_id: ProjectIds,
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
        /\ Len(response.rows) = 0
        /\ response.effects_project_id = "none"

MinMismatchBounded ==
    response.min10 \in 0..10

RowsProjectScoped ==
    ~response.rejected =>
        \A i \in 1..Len(response.rows) : response.rows[i].project_id = response.project_id

SingleFileDirectoriesSuppressed ==
    ~response.rejected =>
        \A i \in 1..Len(response.rows) :
            DirSize(TopicRows(response.project_id), "other") > 1 \/ response.rows[i].path # "other/x.rs"

EffectsUseResolvedProject ==
    ~response.rejected =>
        response.effects_project_id \in {"none", response.project_id}

ProjectOutputNormalized ==
    ~response.rejected /\ req.raw_project = " p " => response.project = "p"

ReadOnlyNoLocks ==
    /\ response.writes = 0
    /\ response.locks = 0

=============================================================================
