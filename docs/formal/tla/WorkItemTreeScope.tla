----------------------------- MODULE WorkItemTreeScope -----------------------------
(***************************************************************************)
(* `work_item_tree` bounded subtree model.                                 *)
(*                                                                         *)
(* The tool resolves a trimmed public id, then runs a recursive read query  *)
(* with an effective row cap. The SQL tracks the visited id path so a       *)
(* corrupted parent cycle cannot generate an unbounded recursive result.    *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

InputKinds == {"valid", "blank", "missing"}
Graphs == {"acyclic", "cycle"}
Outcomes == {"ok", "rejected"}
Rows == {"none", "root", "high", "low", "cycle-child"}

Requests ==
    { [id |-> 1, input |-> "valid", graph |-> "acyclic", max_rows |-> 2],
      [id |-> 2, input |-> "valid", graph |-> "acyclic", max_rows |-> 0],
      [id |-> 3, input |-> "valid", graph |-> "acyclic", max_rows |-> 200000],
      [id |-> 4, input |-> "valid", graph |-> "cycle", max_rows |-> 10],
      [id |-> 5, input |-> "blank", graph |-> "acyclic", max_rows |-> 10],
      [id |-> 6, input |-> "missing", graph |-> "acyclic", max_rows |-> 10] }

RequestIds == {r.id : r \in Requests}

ValidRoot(r) == r.input = "valid"

EffectiveLimit(r) ==
    IF r.max_rows < 1 THEN 1
    ELSE IF r.max_rows > 100000 THEN 100000
    ELSE r.max_rows

AcyclicCount(r) ==
    IF EffectiveLimit(r) < 3 THEN EffectiveLimit(r) ELSE 3

CycleCount(r) ==
    IF EffectiveLimit(r) < 2 THEN EffectiveLimit(r) ELSE 2

RowCountFor(r) ==
    IF ~ValidRoot(r) THEN 0
    ELSE IF r.graph = "cycle" THEN CycleCount(r)
    ELSE AcyclicCount(r)

FirstRowFor(r) ==
    IF RowCountFor(r) >= 1 THEN "root" ELSE "none"

SecondRowFor(r) ==
    IF RowCountFor(r) < 2 THEN "none"
    ELSE IF r.graph = "cycle" THEN "cycle-child"
    ELSE "high"

ThirdRowFor(r) ==
    IF RowCountFor(r) < 3 THEN "none"
    ELSE "low"

ResponseFor(r) ==
    [ request_id |-> r.id,
      outcome |-> IF ValidRoot(r) THEN "ok" ELSE "rejected",
      effective_limit |-> EffectiveLimit(r),
      row_count |-> RowCountFor(r),
      first_row |-> FirstRowFor(r),
      second_row |-> SecondRowFor(r),
      third_row |-> ThirdRowFor(r),
      contains_duplicate |-> FALSE,
      cycle_suppressed |-> r.graph = "cycle" /\ ValidRoot(r),
      stats_query_incremented |-> ValidRoot(r),
      wrote_db |-> FALSE,
      lock_held |-> FALSE ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      effective_limit: 1..100000,
      row_count: 0..3,
      first_row: Rows,
      second_row: Rows,
      third_row: Rows,
      contains_duplicate: BOOLEAN,
      cycle_suppressed: BOOLEAN,
      stats_query_incremented: BOOLEAN,
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

InvalidPublicIdRejects ==
    ~ValidRoot(req) =>
        /\ response.outcome = "rejected"
        /\ response.row_count = 0
        /\ response.first_row = "none"
        /\ ~response.stats_query_incremented

LimitClamped ==
    response.effective_limit \in 1..100000

RowsBoundedByLimit ==
    response.row_count <= response.effective_limit

RootIncludedWhenValid ==
    ValidRoot(req) => response.first_row = "root"

DepthPriorityOrdering ==
    ValidRoot(req) /\ req.graph = "acyclic" /\ response.row_count >= 2 =>
        response.second_row = "high"

CycleSuppressedFinite ==
    ValidRoot(req) /\ req.graph = "cycle" =>
        /\ response.cycle_suppressed
        /\ response.row_count <= 2
        /\ ~response.contains_duplicate

NoDuplicateRows ==
    ~response.contains_duplicate

ReadOnlyNoLock ==
    /\ ~response.wrote_db
    /\ ~response.lock_held

StatsIncrementOnlyOnSuccessfulTreeRead ==
    response.stats_query_incremented = ValidRoot(req)

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidPublicIdRejects /\
        LimitClamped /\
        RowsBoundedByLimit /\
        RootIncludedWhenValid /\
        DepthPriorityOrdering /\
        CycleSuppressedFinite /\
        NoDuplicateRows /\
        ReadOnlyNoLock /\
        StatsIncrementOnlyOnSuccessfulTreeRead)

================================================================================
