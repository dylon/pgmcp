----------------------------- MODULE DocCodeDriftScope -----------------------------
(***************************************************************************)
(* `doc_code_drift` request/scoping/bounding model.                        *)
(*                                                                         *)
(* The tool resolves one unique project, computes directory-level doc/code  *)
(* embedding drift for that resolved project, filters by a normalized       *)
(* cosine-distance threshold, applies a bounded SQL LIMIT, enriches with    *)
(* project-scoped effect counts, and returns without writes or held locks.  *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

ProjectModes == {"valid_unique", "valid_trimmed", "missing", "duplicate", "blank"}
Outcomes == {"ok", "rejected"}

Requests ==
    { [id |-> 1, project |-> "valid_trimmed", raw_min |-> 50,
       raw_limit |-> 5, has_target_effect |-> TRUE],
      [id |-> 2, project |-> "duplicate", raw_min |-> 30,
       raw_limit |-> 30, has_target_effect |-> TRUE],
      [id |-> 3, project |-> "missing", raw_min |-> 30,
       raw_limit |-> 30, has_target_effect |-> TRUE],
      [id |-> 4, project |-> "blank", raw_min |-> 30,
       raw_limit |-> 30, has_target_effect |-> TRUE],
      [id |-> 5, project |-> "valid_unique", raw_min |-> -500,
       raw_limit |-> -10, has_target_effect |-> FALSE],
      [id |-> 6, project |-> "valid_unique", raw_min |-> 999,
       raw_limit |-> 500, has_target_effect |-> TRUE] }

RequestIds == {r.id : r \in Requests}

ValidProject(r) == r.project \in {"valid_unique", "valid_trimmed"}
Accepted(r) == ValidProject(r)

MinDriftFor(r) ==
    IF r.raw_min < 0 THEN 0
    ELSE IF r.raw_min > 200 THEN 200
    ELSE r.raw_min

LimitFor(r) ==
    IF r.raw_limit < 0 THEN 0
    ELSE IF r.raw_limit > 100 THEN 100
    ELSE r.raw_limit

RowsFor(r) ==
    IF ~Accepted(r) THEN 0
    ELSE IF LimitFor(r) = 0 THEN 0
    ELSE IF LimitFor(r) = 1 THEN 1
    ELSE 2

ResponseFor(r) ==
    [ request_id |-> r.id,
      outcome |-> IF Accepted(r) THEN "ok" ELSE "rejected",
      min_drift |-> MinDriftFor(r),
      limit |-> LimitFor(r),
      returned_rows |-> RowsFor(r),
      scanned_target_project |-> Accepted(r),
      scanned_other_project |-> FALSE,
      target_effect_count |-> IF Accepted(r) /\ r.has_target_effect THEN 1 ELSE 0,
      other_project_effect_count |-> 0,
      wrote_db |-> FALSE,
      lock_held |-> FALSE ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      min_drift: 0..200,
      limit: 0..100,
      returned_rows: 0..100,
      scanned_target_project: BOOLEAN,
      scanned_other_project: BOOLEAN,
      target_effect_count: 0..1,
      other_project_effect_count: 0..1,
      wrote_db: BOOLEAN,
      lock_held: BOOLEAN ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    response \in ResponseRecord

InvalidProjectRejects ==
    ~Accepted(req) =>
        /\ response.outcome = "rejected"
        /\ response.returned_rows = 0
        /\ ~response.scanned_target_project
        /\ ~response.scanned_other_project

TrimmedProjectAccepted ==
    req.project = "valid_trimmed" => response.outcome = "ok"

ThresholdBounded ==
    response.min_drift \in 0..200

LimitBounded ==
    response.limit \in 0..100

ReturnedRowsSqlBounded ==
    /\ response.returned_rows <= response.limit
    /\ response.returned_rows <= 100

DriftRowsProjectScoped ==
    /\ response.scanned_target_project = Accepted(req)
    /\ ~response.scanned_other_project

EffectBreakdownResolvedProjectScoped ==
    /\ response.target_effect_count = IF Accepted(req) /\ req.has_target_effect THEN 1 ELSE 0
    /\ response.other_project_effect_count = 0

ReadOnlyNoLock ==
    /\ ~response.wrote_db
    /\ ~response.lock_held

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidProjectRejects /\
        TrimmedProjectAccepted /\
        ThresholdBounded /\
        LimitBounded /\
        ReturnedRowsSqlBounded /\
        DriftRowsProjectScoped /\
        EffectBreakdownResolvedProjectScoped /\
        ReadOnlyNoLock)

================================================================================
