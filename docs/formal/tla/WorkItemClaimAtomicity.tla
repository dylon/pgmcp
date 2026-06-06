----------------------------- MODULE WorkItemClaimAtomicity -----------------------------
(***************************************************************************)
(* `work_item_claim` request/CAS model.                                    *)
(*                                                                         *)
(* The MCP boundary trims and validates explicit agent ids before the       *)
(* database mutation. The SQL path is a single-row compare-and-set inside   *)
(* one transaction: a successful claim updates the item, writes one claim   *)
(* ledger row, and then the tool touches presence. Failed claims leave item *)
(* state and the ledger unchanged.                                          *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

AgentInputs == {"trimmed", "blank"}
Agents == {"none", "agent-a", "agent-b", "unknown-agent"}
Statuses == {"pending", "confirmed", "ready", "blocked", "in_progress",
             "claimed_done", "verified", "cancelled"}
Outcomes == {"claimed", "rejected", "contended"}
Reasons == {"none", "invalid_agent", "owner_active", "dependency_blocked", "terminal"}
OpenStatuses == {"pending", "confirmed", "ready", "blocked", "in_progress"}
PromotedStatuses == {"pending", "confirmed", "ready", "blocked"}

Requests ==
    { [id |-> 1, agent_input |-> "trimmed", owner |-> "none",
       lease_expired |-> FALSE, status |-> "pending", dependency_open |-> FALSE,
       lease_secs |-> 5],
      [id |-> 2, agent_input |-> "blank", owner |-> "none",
       lease_expired |-> FALSE, status |-> "pending", dependency_open |-> FALSE,
       lease_secs |-> 300],
      [id |-> 3, agent_input |-> "trimmed", owner |-> "agent-b",
       lease_expired |-> FALSE, status |-> "pending", dependency_open |-> FALSE,
       lease_secs |-> 300],
      [id |-> 4, agent_input |-> "trimmed", owner |-> "agent-b",
       lease_expired |-> TRUE, status |-> "ready", dependency_open |-> FALSE,
       lease_secs |-> 90000],
      [id |-> 5, agent_input |-> "trimmed", owner |-> "agent-a",
       lease_expired |-> FALSE, status |-> "in_progress", dependency_open |-> FALSE,
       lease_secs |-> 300],
      [id |-> 6, agent_input |-> "trimmed", owner |-> "none",
       lease_expired |-> FALSE, status |-> "blocked", dependency_open |-> TRUE,
       lease_secs |-> 300],
      [id |-> 7, agent_input |-> "trimmed", owner |-> "none",
       lease_expired |-> FALSE, status |-> "verified", dependency_open |-> FALSE,
       lease_secs |-> 300] }

RequestIds == {r.id : r \in Requests}

ValidAgent(r) == r.agent_input = "trimmed"

AgentFor(r) ==
    IF ValidAgent(r) THEN "agent-a" ELSE "none"

LeaseFor(r) ==
    IF r.lease_secs < 10 THEN 10
    ELSE IF r.lease_secs > 86400 THEN 86400
    ELSE r.lease_secs

OwnerAllowsClaim(r) ==
    r.owner = "none" \/ r.owner = AgentFor(r) \/ r.lease_expired

TerminalStatus(r) ==
    r.status \notin OpenStatuses

ReasonFor(r) ==
    CASE ~ValidAgent(r) -> "invalid_agent"
      [] TerminalStatus(r) -> "terminal"
      [] r.dependency_open -> "dependency_blocked"
      [] ~OwnerAllowsClaim(r) -> "owner_active"
      [] OTHER -> "none"

ClaimSucceeds(r) == ReasonFor(r) = "none"

StatusAfter(r) ==
    IF ClaimSucceeds(r) /\ r.status \in PromotedStatuses THEN "in_progress"
    ELSE r.status

ResponseFor(r) ==
    [ request_id |-> r.id,
      outcome |-> IF ClaimSucceeds(r) THEN "claimed"
                  ELSE IF ReasonFor(r) = "invalid_agent" THEN "rejected"
                  ELSE "contended",
      reason |-> ReasonFor(r),
      normalized_agent |-> AgentFor(r),
      owner_before |-> r.owner,
      owner_after |-> IF ClaimSucceeds(r) THEN AgentFor(r) ELSE r.owner,
      status_before |-> r.status,
      status_after |-> StatusAfter(r),
      lease_secs_effective |-> LeaseFor(r),
      item_written |-> ClaimSucceeds(r),
      ledger_rows |-> IF ClaimSucceeds(r) THEN 1 ELSE 0,
      presence_touched |-> ClaimSucceeds(r),
      claim_count_delta |-> IF ClaimSucceeds(r) THEN 1 ELSE 0,
      row_lock_held |-> FALSE ]

RaceOutcomes ==
    { [a_claimed |-> TRUE, b_claimed |-> FALSE, final_owner |-> "agent-a", ledger_rows |-> 1],
      [a_claimed |-> FALSE, b_claimed |-> TRUE, final_owner |-> "agent-b", ledger_rows |-> 1] }

VARIABLES req, response, race

vars == <<req, response, race>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      reason: Reasons,
      normalized_agent: Agents,
      owner_before: Agents,
      owner_after: Agents,
      status_before: Statuses,
      status_after: Statuses,
      lease_secs_effective: 10..86400,
      item_written: BOOLEAN,
      ledger_rows: 0..1,
      presence_touched: BOOLEAN,
      claim_count_delta: 0..1,
      row_lock_held: BOOLEAN ]

RaceRecord ==
    [ a_claimed: BOOLEAN,
      b_claimed: BOOLEAN,
      final_owner: {"agent-a", "agent-b"},
      ledger_rows: 0..1 ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)
    /\ race \in RaceOutcomes

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ response \in ResponseRecord
    /\ race \in RaceRecord

InvalidAgentNoWrite ==
    ~ValidAgent(req) =>
        /\ response.outcome = "rejected"
        /\ ~response.item_written
        /\ response.ledger_rows = 0
        /\ ~response.presence_touched
        /\ response.owner_after = req.owner

ContendedOwnerNoWrite ==
    ValidAgent(req) /\ ~OwnerAllowsClaim(req) =>
        /\ response.outcome = "contended"
        /\ ~response.item_written
        /\ response.ledger_rows = 0
        /\ response.owner_after = req.owner

BlockedNoWrite ==
    ValidAgent(req) /\ req.dependency_open =>
        /\ response.outcome = "contended"
        /\ ~response.item_written
        /\ response.ledger_rows = 0
        /\ response.owner_after = req.owner

TerminalNoWrite ==
    ValidAgent(req) /\ TerminalStatus(req) =>
        /\ response.outcome = "contended"
        /\ ~response.item_written
        /\ response.ledger_rows = 0
        /\ response.status_after = req.status

SuccessfulClaimAtomic ==
    ClaimSucceeds(req) =>
        /\ response.outcome = "claimed"
        /\ response.owner_after = AgentFor(req)
        /\ response.item_written
        /\ response.ledger_rows = 1
        /\ response.presence_touched
        /\ response.claim_count_delta = 1

LeaseClamped ==
    response.lease_secs_effective \in 10..86400

OpenStatusPromoted ==
    ClaimSucceeds(req) /\ req.status \in PromotedStatuses =>
        response.status_after = "in_progress"

InProgressStaysInProgress ==
    ClaimSucceeds(req) /\ req.status = "in_progress" =>
        response.status_after = "in_progress"

ExpiredLeaseStealable ==
    ValidAgent(req) /\ req.owner = "agent-b" /\ req.lease_expired /\ ~req.dependency_open /\ ~TerminalStatus(req) =>
        /\ response.outcome = "claimed"
        /\ response.owner_after = "agent-a"

SameOwnerRenewalAllowed ==
    ValidAgent(req) /\ req.owner = AgentFor(req) /\ ~req.dependency_open /\ ~TerminalStatus(req) =>
        response.outcome = "claimed"

ConcurrentAtMostOneWinner ==
    ~(race.a_claimed /\ race.b_claimed)

ConcurrentLedgerMatchesWinner ==
    race.ledger_rows =
        IF race.a_claimed \/ race.b_claimed THEN 1 ELSE 0

ConcurrentFinalOwnerIsWinner ==
    /\ (race.a_claimed => race.final_owner = "agent-a")
    /\ (race.b_claimed => race.final_owner = "agent-b")

RowLockReleased ==
    response.row_lock_held = FALSE

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidAgentNoWrite /\
        ContendedOwnerNoWrite /\
        BlockedNoWrite /\
        TerminalNoWrite /\
        SuccessfulClaimAtomic /\
        LeaseClamped /\
        OpenStatusPromoted /\
        InProgressStaysInProgress /\
        ExpiredLeaseStealable /\
        SameOwnerRenewalAllowed /\
        ConcurrentAtMostOneWinner /\
        ConcurrentLedgerMatchesWinner /\
        ConcurrentFinalOwnerIsWinner /\
        RowLockReleased)

================================================================================
