------------------------- MODULE DeadlockAnalysisBoundary -------------------------
(***************************************************************************)
(* MCP request/response boundary for `deadlock_cycles`, `lock_order_graph`, *)
(* and `channel_deadlock`. The operational deadlock theorems live in        *)
(* LockOrderDeadlock.{tla,v} and ChannelDeadlock.{tla,v}; this model checks *)
(* the tool wrapper obligations: fail-closed project resolution, finite     *)
(* confidence normalization, bounded result windows, scoped rows, read-only *)
(* execution, and no retained locks.                                        *)
(***************************************************************************)

EXTENDS Naturals, FiniteSets, TLC

MaxDepth == 12
MaxCycleLen == 12
MaxLimit == 500

Projects == {1, 2, 3, 4}
ProjectName ==
    ( 1 :> "target"
   @@ 2 :> "other"
   @@ 3 :> "duplicate"
   @@ 4 :> "duplicate" )

LockRows == {101, 102, 201}
ChannelRows == {301, 401}
RowProject == (101 :> 1 @@ 102 :> 1 @@ 201 :> 2 @@ 301 :> 1 @@ 401 :> 2)

NoReq == [
    tool |-> "none",
    project |-> "",
    confidence |-> "default",
    depth |-> 0,
    cycle_len |-> 0,
    limit |-> 0
]

Requests == {
    [tool |-> "deadlock_cycles", project |-> " target ", confidence |-> "high", depth |-> 99, cycle_len |-> 1, limit |-> 0 - 50],
    [tool |-> "deadlock_cycles", project |-> "target", confidence |-> "nan", depth |-> 5, cycle_len |-> 6, limit |-> 50],
    [tool |-> "lock_order_graph", project |-> "target", confidence |-> "low", depth |-> 0, cycle_len |-> 0, limit |-> 0],
    [tool |-> "channel_deadlock", project |-> "target", confidence |-> "default", depth |-> 0, cycle_len |-> 0, limit |-> 5000],
    [tool |-> "channel_deadlock", project |-> "duplicate", confidence |-> "default", depth |-> 0, cycle_len |-> 0, limit |-> 50],
    [tool |-> "channel_deadlock", project |-> "", confidence |-> "default", depth |-> 0, cycle_len |-> 0, limit |-> 50]
}

VARIABLES req, status, resolved_project, effective_confidence, effective_depth,
          effective_cycle_len, effective_limit, output_rows, writes, locks_held

vars == <<req, status, resolved_project, effective_confidence, effective_depth,
          effective_cycle_len, effective_limit, output_rows, writes, locks_held>>

TrimProject(raw) ==
    CASE raw = " target " -> "target"
      [] OTHER -> raw

ProjectMatches(name) == {p \in Projects : ProjectName[p] = name}

\* Confidence is represented as tenths: 3 = default 0.3, 10 = 1.0.
FiniteConfidence(c) == c # "nan"
ClampConfidence(c) ==
    CASE c = "low" -> 0
      [] c = "high" -> 10
      [] OTHER -> 3

ClampDepth(n) ==
    IF n < 1 THEN 1 ELSE IF n > MaxDepth THEN MaxDepth ELSE n

ClampCycleLen(n) ==
    IF n < 2 THEN 2 ELSE IF n > MaxCycleLen THEN MaxCycleLen ELSE n

ClampLimit(n) ==
    IF n < 1 THEN 1 ELSE IF n > MaxLimit THEN MaxLimit ELSE n

RowsFor(tool, project) ==
    IF tool = "channel_deadlock"
    THEN {r \in ChannelRows : RowProject[r] = project}
    ELSE {r \in LockRows : RowProject[r] = project}

Init ==
    /\ req = NoReq
    /\ status = "idle"
    /\ resolved_project = 0
    /\ effective_confidence = 0
    /\ effective_depth = 0
    /\ effective_cycle_len = 0
    /\ effective_limit = 0
    /\ output_rows = {}
    /\ writes = {}
    /\ locks_held = {}

PickRequest(r) ==
    /\ status = "idle"
    /\ r \in Requests
    /\ req' = r
    /\ status' = "pending"
    /\ resolved_project' = 0
    /\ effective_confidence' = 0
    /\ effective_depth' = 0
    /\ effective_cycle_len' = 0
    /\ effective_limit' = 0
    /\ output_rows' = {}
    /\ writes' = {}
    /\ locks_held' = {}

Respond ==
    /\ status = "pending"
    /\ LET name == TrimProject(req.project) IN
       LET matches == ProjectMatches(name) IN
       IF name = "" THEN
          /\ status' = "invalid"
          /\ resolved_project' = 0
          /\ effective_confidence' = 0
          /\ effective_depth' = 0
          /\ effective_cycle_len' = 0
          /\ effective_limit' = 0
          /\ output_rows' = {}
       ELSE IF ~FiniteConfidence(req.confidence) THEN
          /\ status' = "invalid"
          /\ resolved_project' = 0
          /\ effective_confidence' = 0
          /\ effective_depth' = 0
          /\ effective_cycle_len' = 0
          /\ effective_limit' = 0
          /\ output_rows' = {}
       ELSE IF Cardinality(matches) = 0 THEN
          /\ status' = "not_found"
          /\ resolved_project' = 0
          /\ effective_confidence' = 0
          /\ effective_depth' = 0
          /\ effective_cycle_len' = 0
          /\ effective_limit' = 0
          /\ output_rows' = {}
       ELSE IF Cardinality(matches) > 1 THEN
          /\ status' = "ambiguous"
          /\ resolved_project' = 0
          /\ effective_confidence' = 0
          /\ effective_depth' = 0
          /\ effective_cycle_len' = 0
          /\ effective_limit' = 0
          /\ output_rows' = {}
       ELSE
          /\ status' = "ok"
          /\ resolved_project' = CHOOSE p \in matches : TRUE
          /\ effective_confidence' = ClampConfidence(req.confidence)
          /\ effective_depth' =
                IF req.tool = "channel_deadlock" THEN 0 ELSE ClampDepth(req.depth)
          /\ effective_cycle_len' =
                IF req.tool = "deadlock_cycles" THEN ClampCycleLen(req.cycle_len) ELSE 0
          /\ effective_limit' =
                IF req.tool = "lock_order_graph" THEN 0 ELSE ClampLimit(req.limit)
          /\ output_rows' = RowsFor(req.tool, resolved_project')
    /\ writes' = {}
    /\ locks_held' = {}
    /\ UNCHANGED req

Reset ==
    /\ status \in {"ok", "invalid", "not_found", "ambiguous"}
    /\ req' = NoReq
    /\ status' = "idle"
    /\ resolved_project' = 0
    /\ effective_confidence' = 0
    /\ effective_depth' = 0
    /\ effective_cycle_len' = 0
    /\ effective_limit' = 0
    /\ output_rows' = {}
    /\ writes' = {}
    /\ locks_held' = {}

Next ==
    \/ \E r \in Requests : PickRequest(r)
    \/ Respond
    \/ Reset

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ status \in {"idle", "pending", "ok", "invalid", "not_found", "ambiguous"}
    /\ resolved_project \in Projects \cup {0}
    /\ effective_confidence \in 0..10
    /\ effective_depth \in 0..MaxDepth
    /\ effective_cycle_len \in 0..MaxCycleLen
    /\ effective_limit \in 0..MaxLimit
    /\ output_rows \subseteq LockRows \cup ChannelRows
    /\ writes = {}
    /\ locks_held = {}

InvalidNoScan ==
    status \in {"invalid", "not_found", "ambiguous"}
    => /\ resolved_project = 0
       /\ output_rows = {}
       /\ effective_limit = 0

FiniteConfidenceOnly ==
    status = "ok" => effective_confidence \in 0..10

BoundedDeadlockParams ==
    /\ status = "ok"
    /\ req.tool = "deadlock_cycles"
    => /\ effective_depth \in 1..MaxDepth
       /\ effective_cycle_len \in 2..MaxCycleLen
       /\ effective_limit \in 1..MaxLimit

BoundedChannelParams ==
    /\ status = "ok"
    /\ req.tool = "channel_deadlock"
    => effective_limit \in 1..MaxLimit

RowsScoped ==
    status = "ok" => \A r \in output_rows : RowProject[r] = resolved_project

ReadOnlyAndNoLocksHeld ==
    /\ writes = {}
    /\ locks_held = {}

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidNoScan /\
        FiniteConfidenceOnly /\
        BoundedDeadlockParams /\
        BoundedChannelParams /\
        RowsScoped /\
        ReadOnlyAndNoLocksHeld)

=============================================================================
