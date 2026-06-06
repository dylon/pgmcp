------------------------ MODULE WorkItemSetStatusAtomicity ------------------------
(***************************************************************************)
(* `work_item_set_status` request and transaction boundary.                 *)
(*                                                                         *)
(* The MCP tool always acts as Actor::Agent. The query layer must lock the  *)
(* work_items row, read the current status and evidence context inside that *)
(* transaction, validate the transition against the locked status, and write *)
(* the item status plus history row atomically.                             *)
(*                                                                         *)
(* Verified:                                                               *)
(*   - blank public ids and statuses are rejected before mutation;          *)
(*   - unknown statuses are rejected before mutation;                       *)
(*   - MCP-authored status history rows are always agent rows;              *)
(*   - agents cannot write verified/deferred/rejected judgment statuses;    *)
(*   - successful status mutations and history rows are atomic;             *)
(*   - racing pending->triage and pending->in_progress requests serialize   *)
(*     through a row-lock recheck so at most one observes pending.           *)
(***************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

Statuses ==
    {"pending", "triage", "confirmed", "ready", "in_progress", "blocked",
     "claimed_done", "verifying", "verified", "rejected", "deferred",
     "cancelled"}

JudgmentStatuses == {"verified", "rejected", "deferred"}
Phases == {"idle", "pending", "done"}
Kinds == {"single", "race"}
Orders == {"none", "triage_first", "progress_first"}
Reasons == {"none", "blank_public", "blank_status", "unknown_status", "transition_refused"}
HistoryReasons == {"none", "start", "race to triage", "race to progress"}

NoReq ==
    [ id |-> 0,
      kind |-> "single",
      raw_public |-> "WI-1",
      raw_status |-> "pending",
      raw_reason |-> "",
      initial |-> "pending",
      order |-> "none" ]

Requests ==
    { [id |-> 1, kind |-> "single", raw_public |-> "", raw_status |-> "in_progress",
       raw_reason |-> "start", initial |-> "pending", order |-> "none"],
      [id |-> 2, kind |-> "single", raw_public |-> "   ", raw_status |-> "in_progress",
       raw_reason |-> "start", initial |-> "pending", order |-> "none"],
      [id |-> 3, kind |-> "single", raw_public |-> "WI-1", raw_status |-> "",
       raw_reason |-> "start", initial |-> "pending", order |-> "none"],
      [id |-> 4, kind |-> "single", raw_public |-> "WI-1", raw_status |-> "done",
       raw_reason |-> "start", initial |-> "pending", order |-> "none"],
      [id |-> 5, kind |-> "single", raw_public |-> " WI-1 ", raw_status |-> " in_progress ",
       raw_reason |-> " start ", initial |-> "pending", order |-> "none"],
      [id |-> 6, kind |-> "single", raw_public |-> "WI-1", raw_status |-> "verified",
       raw_reason |-> "   ", initial |-> "claimed_done", order |-> "none"],
      [id |-> 7, kind |-> "single", raw_public |-> "WI-1", raw_status |-> "deferred",
       raw_reason |-> "   ", initial |-> "pending", order |-> "none"],
      [id |-> 8, kind |-> "single", raw_public |-> "WI-1", raw_status |-> "rejected",
       raw_reason |-> "   ", initial |-> "claimed_done", order |-> "none"],
      [id |-> 9, kind |-> "race", raw_public |-> "WI-1", raw_status |-> "race",
       raw_reason |-> "", initial |-> "pending", order |-> "triage_first"],
      [id |-> 10, kind |-> "race", raw_public |-> "WI-1", raw_status |-> "race",
       raw_reason |-> "", initial |-> "pending", order |-> "progress_first"] }

RequestIds == {r.id : r \in Requests} \cup {101, 102}

NormalizePublic(raw) ==
    CASE raw = " WI-1 " -> "WI-1"
      [] raw = "   " -> ""
      [] OTHER -> raw

NormalizeStatus(raw) ==
    CASE raw = " in_progress " -> "in_progress"
      [] raw = "   " -> ""
      [] OTHER -> raw

NormalizeReason(raw) ==
    CASE raw = " start " -> "start"
      [] raw = "   " -> "none"
      [] raw = "" -> "none"
      [] OTHER -> raw

AgentAllowed(from, to) ==
    \/ /\ from = "pending"
       /\ to \in {"ready", "in_progress", "blocked", "triage"}
    \/ /\ from = "triage"
       /\ to = "blocked"
    \/ /\ from = "confirmed"
       /\ to \in {"in_progress", "ready", "blocked"}
    \/ /\ from = "ready"
       /\ to \in {"in_progress", "blocked"}
    \/ /\ from = "in_progress"
       /\ to \in {"blocked", "claimed_done", "verifying"}
    \/ /\ from = "blocked"
       /\ to \in {"ready", "in_progress"}
    \/ /\ from = "claimed_done"
       /\ to \in {"in_progress", "verifying"}
    \/ /\ from = "verifying"
       /\ to = "in_progress"
    \/ /\ from = "verified"
       /\ to \in {"in_progress", "triage"}
    \/ /\ from = "rejected"
       /\ to \in {"in_progress", "blocked", "claimed_done", "verifying"}

ResultFor(r, current) ==
    LET public == NormalizePublic(r.raw_public) IN
    LET target0 == NormalizeStatus(r.raw_status) IN
    LET target == IF target0 \in Statuses THEN target0 ELSE "none" IN
        CASE public = "" ->
            [ request_id |-> r.id,
              applied |-> FALSE,
              reason |-> "blank_public",
              from |-> current,
              to |-> target ]
          [] target0 = "" ->
            [ request_id |-> r.id,
              applied |-> FALSE,
              reason |-> "blank_status",
              from |-> current,
              to |-> target ]
          [] ~(target0 \in Statuses) ->
            [ request_id |-> r.id,
              applied |-> FALSE,
              reason |-> "unknown_status",
              from |-> current,
              to |-> "none" ]
          [] ~AgentAllowed(current, target0) ->
            [ request_id |-> r.id,
              applied |-> FALSE,
              reason |-> "transition_refused",
              from |-> current,
              to |-> target ]
          [] OTHER ->
            [ request_id |-> r.id,
              applied |-> TRUE,
              reason |-> "none",
              from |-> current,
              to |-> target ]

StatusAfter(current, result) ==
    IF result.applied THEN result.to ELSE current

HistoryRow(result, reason) ==
    [ from |-> result.from,
      to |-> result.to,
      actor |-> "agent",
      reason |-> reason ]

RaceTriageReq ==
    [ id |-> 101,
      kind |-> "single",
      raw_public |-> "WI-1",
      raw_status |-> "triage",
      raw_reason |-> "race to triage",
      initial |-> "pending",
      order |-> "none" ]

RaceProgressReq ==
    [ id |-> 102,
      kind |-> "single",
      raw_public |-> "WI-1",
      raw_status |-> "in_progress",
      raw_reason |-> "race to progress",
      initial |-> "pending",
      order |-> "none" ]

FirstRaceReq(order) ==
    IF order = "triage_first" THEN RaceTriageReq ELSE RaceProgressReq

SecondRaceReq(order) ==
    IF order = "triage_first" THEN RaceProgressReq ELSE RaceTriageReq

RaceResults(order, current) ==
    LET first == FirstRaceReq(order) IN
    LET second == SecondRaceReq(order) IN
    LET r1 == ResultFor(first, current) IN
    LET s1 == StatusAfter(current, r1) IN
    LET r2 == ResultFor(second, s1) IN
        <<r1, r2>>

NoResp ==
    [ rejected |-> FALSE,
      reason |-> "none",
      results |-> <<>> ]

VARIABLES req, phase, itemStatus, history, resp

vars == <<req, phase, itemStatus, history, resp>>

Init ==
    /\ req = NoReq
    /\ phase = "idle"
    /\ itemStatus = "pending"
    /\ history = <<>>
    /\ resp = NoResp

PickRequest(r) ==
    /\ phase = "idle"
    /\ r \in Requests
    /\ req' = r
    /\ phase' = "pending"
    /\ itemStatus' = r.initial
    /\ history' = <<>>
    /\ resp' = NoResp

ProcessSingle ==
    /\ phase = "pending"
    /\ req.kind = "single"
    /\ LET result == ResultFor(req, itemStatus) IN
       /\ resp' =
            [ rejected |-> ~result.applied,
              reason |-> result.reason,
              results |-> <<result>> ]
       /\ itemStatus' = StatusAfter(itemStatus, result)
       /\ history' =
            IF result.applied
            THEN <<HistoryRow(result, NormalizeReason(req.raw_reason))>>
            ELSE <<>>
    /\ phase' = "done"
    /\ UNCHANGED req

ProcessRace ==
    /\ phase = "pending"
    /\ req.kind = "race"
    /\ LET first == FirstRaceReq(req.order) IN
       LET second == SecondRaceReq(req.order) IN
       LET results == RaceResults(req.order, itemStatus) IN
       LET h1 ==
            IF results[1].applied
            THEN <<HistoryRow(results[1], NormalizeReason(first.raw_reason))>>
            ELSE <<>> IN
       LET h2 ==
            IF results[2].applied
            THEN <<HistoryRow(results[2], NormalizeReason(second.raw_reason))>>
            ELSE <<>> IN
       /\ resp' =
            [ rejected |-> FALSE,
              reason |-> "none",
              results |-> results ]
       /\ itemStatus' = StatusAfter(StatusAfter(itemStatus, results[1]), results[2])
       /\ history' = h1 \o h2
    /\ phase' = "done"
    /\ UNCHANGED req

TerminalStutter ==
    /\ phase = "done"
    /\ UNCHANGED vars

Next ==
    \/ \E r \in Requests : PickRequest(r)
    \/ ProcessSingle
    \/ ProcessRace
    \/ TerminalStutter

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

AppliedResultIndexes ==
    {i \in 1..Len(resp.results) : resp.results[i].applied}

PendingHistoryIndexes ==
    {i \in 1..Len(history) : history[i].from = "pending"}

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ phase \in Phases
    /\ itemStatus \in Statuses
    /\ Len(history) \in 0..2
    /\ \A i \in 1..Len(history) :
        /\ history[i].from \in Statuses
        /\ history[i].to \in Statuses
        /\ history[i].actor = "agent"
        /\ history[i].reason \in HistoryReasons
    /\ resp.rejected \in BOOLEAN
    /\ resp.reason \in Reasons
    /\ Len(resp.results) \in 0..2
    /\ \A i \in 1..Len(resp.results) :
        /\ resp.results[i].request_id \in RequestIds
        /\ resp.results[i].applied \in BOOLEAN
        /\ resp.results[i].reason \in Reasons
        /\ resp.results[i].from \in Statuses
        /\ resp.results[i].to \in Statuses \cup {"none"}

BlankPublicRejected ==
    phase = "done" /\ NormalizePublic(req.raw_public) = "" =>
        /\ resp.rejected
        /\ resp.reason = "blank_public"
        /\ Len(history) = 0
        /\ itemStatus = req.initial

BlankStatusRejected ==
    phase = "done" /\ req.kind = "single" /\ NormalizeStatus(req.raw_status) = "" =>
        /\ resp.rejected
        /\ resp.reason = "blank_status"
        /\ Len(history) = 0
        /\ itemStatus = req.initial

UnknownStatusRejected ==
    phase = "done" /\ req.kind = "single" /\ NormalizeStatus(req.raw_status) # "" /\
    ~(NormalizeStatus(req.raw_status) \in Statuses) =>
        /\ resp.rejected
        /\ resp.reason = "unknown_status"
        /\ Len(history) = 0
        /\ itemStatus = req.initial

HistoryRowsUseAgentActor ==
    \A i \in 1..Len(history) : history[i].actor = "agent"

HistoryRowsAreLegalAgentTransitions ==
    \A i \in 1..Len(history) : AgentAllowed(history[i].from, history[i].to)

AgentNeverWritesJudgmentStatus ==
    \A i \in 1..Len(history) : history[i].to \notin JudgmentStatuses

HistoryAtomicWithItemStatus ==
    /\ (Len(history) = 0 => itemStatus = req.initial)
    /\ (Len(history) > 0 => itemStatus = history[Len(history)].to)

RaceRequestsSerialize ==
    phase = "done" /\ req.kind = "race" =>
        /\ Len(resp.results) = 2
        /\ resp.results[1].from = "pending"
        /\ resp.results[2].from =
            IF resp.results[1].applied THEN resp.results[1].to ELSE "pending"
        /\ Cardinality(AppliedResultIndexes) = 1
        /\ Len(history) = 1

AtMostOnePendingTransitionCommits ==
    Cardinality(PendingHistoryIndexes) <= 1

StoredReasonsAreNormalized ==
    \A i \in 1..Len(history) : history[i].reason \in HistoryReasons

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        BlankPublicRejected /\
        BlankStatusRejected /\
        UnknownStatusRejected /\
        HistoryRowsUseAgentActor /\
        HistoryRowsAreLegalAgentTransitions /\
        AgentNeverWritesJudgmentStatus /\
        HistoryAtomicWithItemStatus /\
        RaceRequestsSerialize /\
        AtMostOnePendingTransitionCommits /\
        StoredReasonsAreNormalized)

=============================================================================
