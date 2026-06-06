----------------------------- MODULE ExperimentListScope -----------------------------
(***************************************************************************)
(* `experiment_list` filter/pagination/read-only model.                    *)
(*                                                                         *)
(* The tool normalizes optional project/kind/status filters, rejects        *)
(* unknown enum values and non-positive project ids, clamps pagination,     *)
(* reads active experiments newest-first, and returns without writes or     *)
(* held locks.                                                             *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

ProjectFilters == {"none", "positive", "zero", "negative"}
KindFilters == {"none", "blank", "valid", "valid_trimmed", "invalid"}
StatusFilters == {"none", "blank", "valid", "valid_trimmed", "invalid"}
Outcomes == {"ok", "rejected"}

Requests ==
    { [id |-> 1, project |-> "positive", kind |-> "valid_trimmed",
       status |-> "valid_trimmed", raw_limit |-> 999, raw_offset |-> -10],
      [id |-> 2, project |-> "zero", kind |-> "valid",
       status |-> "valid", raw_limit |-> 50, raw_offset |-> 0],
      [id |-> 3, project |-> "negative", kind |-> "valid",
       status |-> "valid", raw_limit |-> 50, raw_offset |-> 0],
      [id |-> 4, project |-> "positive", kind |-> "invalid",
       status |-> "valid", raw_limit |-> 50, raw_offset |-> 0],
      [id |-> 5, project |-> "positive", kind |-> "valid",
       status |-> "invalid", raw_limit |-> 50, raw_offset |-> 0],
      [id |-> 6, project |-> "positive", kind |-> "blank",
       status |-> "blank", raw_limit |-> -10, raw_offset |-> 1],
      [id |-> 7, project |-> "none", kind |-> "none",
       status |-> "none", raw_limit |-> 2, raw_offset |-> 1] }

RequestIds == {r.id : r \in Requests}

ProjectOK(r) == r.project \in {"none", "positive"}
KindOK(r) == r.kind \in {"none", "blank", "valid", "valid_trimmed"}
StatusOK(r) == r.status \in {"none", "blank", "valid", "valid_trimmed"}
Accepted(r) == ProjectOK(r) /\ KindOK(r) /\ StatusOK(r)

ProjectFilterPresent(r) == r.project = "positive"
KindFilterPresent(r) == r.kind \in {"valid", "valid_trimmed"}
StatusFilterPresent(r) == r.status \in {"valid", "valid_trimmed"}

LimitFor(r) ==
    IF r.raw_limit < 1 THEN 1
    ELSE IF r.raw_limit > 500 THEN 500
    ELSE r.raw_limit

OffsetFor(r) ==
    IF r.raw_offset < 0 THEN 0 ELSE r.raw_offset

RowsFor(r) ==
    IF ~Accepted(r) THEN 0
    ELSE IF LimitFor(r) = 1 THEN 1
    ELSE 2

ResponseFor(r) ==
    [ request_id |-> r.id,
      outcome |-> IF Accepted(r) THEN "ok" ELSE "rejected",
      limit |-> LimitFor(r),
      offset |-> OffsetFor(r),
      project_filter_present |-> ProjectFilterPresent(r),
      kind_filter_present |-> KindFilterPresent(r),
      status_filter_present |-> StatusFilterPresent(r),
      returned_rows |-> RowsFor(r),
      returned_cross_project_row |-> FALSE,
      newest_first_page |-> Accepted(r),
      wrote_db |-> FALSE,
      lock_held |-> FALSE ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      limit: 1..500,
      offset: Nat,
      project_filter_present: BOOLEAN,
      kind_filter_present: BOOLEAN,
      status_filter_present: BOOLEAN,
      returned_rows: 0..500,
      returned_cross_project_row: BOOLEAN,
      newest_first_page: BOOLEAN,
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

InvalidFiltersReject ==
    ~Accepted(req) =>
        /\ response.outcome = "rejected"
        /\ response.returned_rows = 0
        /\ ~response.newest_first_page

BlankFiltersOmitted ==
    (req.kind = "blank" /\ req.status = "blank") =>
        /\ ~response.kind_filter_present
        /\ ~response.status_filter_present

TrimmedFiltersAccepted ==
    (req.kind = "valid_trimmed" /\ req.status = "valid_trimmed") =>
        response.outcome = "ok"

LimitAndOffsetBounded ==
    /\ response.limit \in 1..500
    /\ response.offset >= 0

ReturnedRowsBounded ==
    /\ response.returned_rows <= response.limit
    /\ response.returned_rows <= 500

ProjectScopeHonored ==
    response.project_filter_present => ~response.returned_cross_project_row

NewestFirstPagination ==
    response.newest_first_page = Accepted(req)

ReadOnlyNoLock ==
    /\ ~response.wrote_db
    /\ ~response.lock_held

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidFiltersReject /\
        BlankFiltersOmitted /\
        TrimmedFiltersAccepted /\
        LimitAndOffsetBounded /\
        ReturnedRowsBounded /\
        ProjectScopeHonored /\
        NewestFirstPagination /\
        ReadOnlyNoLock)

================================================================================
