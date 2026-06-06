--------------------------- MODULE WorkItemProgressLog ---------------------------
(***************************************************************************)
(* Work-item progress log contract for `work_item_record_progress`.         *)
(*                                                                         *)
(* The MCP tool is an agent-authored append-only activity log. It may update *)
(* `claimed_percent`, which is an agent self-report, but it must never       *)
(* create trusted verification progress.                                    *)
(*                                                                         *)
(* Verified:                                                               *)
(*   - empty notes are rejected and never recorded;                         *)
(*   - every MCP-authored progress row has provenance `agent_write`;         *)
(*   - percent values are clamped into [0, 100];                             *)
(*   - recording progress never changes verified roll-up state.              *)
(***************************************************************************)

EXTENDS Integers, Sequences, FiniteSets

NoPercent == -999

NoReq == [id |-> 0, note |-> "", percent |-> NoPercent]

Requests ==
    { [id |-> 1, note |-> "", percent |-> NoPercent],
      [id |-> 2, note |-> "wired", percent |-> 40],
      [id |-> 3, note |-> "too high", percent |-> 250],
      [id |-> 4, note |-> "too low", percent |-> -5],
      [id |-> 5, note |-> "no percent", percent |-> NoPercent] }

RequestIds == {r.id : r \in Requests}
Notes == {"wired", "too high", "too low", "no percent"}
ClampedPercentValues == {0, 40, 100}
PercentValues == ClampedPercentValues \cup {NoPercent}

ProgressRows ==
    [ request_id: RequestIds,
      note: Notes,
      percent: PercentValues,
      provenance: {"agent_write"} ]

VARIABLES req, status, progress, claimedPercent, verifiedLeafCount, seen

vars == <<req, status, progress, claimedPercent, verifiedLeafCount, seen>>

HasPercent(r) == r.percent # NoPercent

Clamp(p) ==
    IF p < 0 THEN 0
    ELSE IF p > 100 THEN 100
    ELSE p

Init ==
    /\ req = NoReq
    /\ status = "idle"
    /\ progress = <<>>
    /\ claimedPercent = NoPercent
    /\ verifiedLeafCount = 0
    /\ seen = {}

PickRequest(r) ==
    /\ status = "idle"
    /\ r \in Requests
    /\ r.id \notin seen
    /\ req' = r
    /\ status' = "pending"
    /\ UNCHANGED <<progress, claimedPercent, verifiedLeafCount, seen>>

RejectEmptyNote ==
    /\ status = "pending"
    /\ req.note = ""
    /\ status' = "rejected"
    /\ seen' = seen \cup {req.id}
    /\ UNCHANGED <<req, progress, claimedPercent, verifiedLeafCount>>

AppendProgress ==
    /\ status = "pending"
    /\ req.note # ""
    /\ LET pct == IF HasPercent(req) THEN Clamp(req.percent) ELSE NoPercent IN
       /\ progress' =
            Append(progress,
                [ request_id |-> req.id,
                  note |-> req.note,
                  percent |-> pct,
                  provenance |-> "agent_write" ])
       /\ claimedPercent' =
            IF HasPercent(req) THEN pct ELSE claimedPercent
    /\ status' = "ok"
    /\ seen' = seen \cup {req.id}
    /\ UNCHANGED <<req, verifiedLeafCount>>

Reset ==
    /\ status \in {"ok", "rejected"}
    /\ req' = NoReq
    /\ status' = "idle"
    /\ UNCHANGED <<progress, claimedPercent, verifiedLeafCount, seen>>

Next ==
    \/ \E r \in Requests : PickRequest(r)
    \/ RejectEmptyNote
    \/ AppendProgress
    \/ Reset

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ status \in {"idle", "pending", "ok", "rejected"}
    /\ progress \in Seq(ProgressRows)
    /\ claimedPercent \in PercentValues
    /\ verifiedLeafCount \in Nat
    /\ seen \subseteq RequestIds

NoEmptyNoteRecorded ==
    \A i \in 1..Len(progress) : progress[i].note # ""

EmptyRequestNeverRecorded ==
    \A i \in 1..Len(progress) : progress[i].request_id # 1

McpProgressAlwaysAgentWrite ==
    \A i \in 1..Len(progress) : progress[i].provenance = "agent_write"

ProgressPercentClamped ==
    \A i \in 1..Len(progress) :
        progress[i].percent \in PercentValues

ClaimedPercentClamped ==
    claimedPercent \in PercentValues

ProgressDoesNotVerify ==
    verifiedLeafCount = 0

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        NoEmptyNoteRecorded /\
        EmptyRequestNeverRecorded /\
        McpProgressAlwaysAgentWrite /\
        ProgressPercentClamped /\
        ClaimedPercentClamped /\
        ProgressDoesNotVerify)

=============================================================================
