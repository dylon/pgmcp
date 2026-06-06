------------------------------ MODULE MemoryOpenNodesScope ------------------------------
(***************************************************************************)
(* `memory_open_nodes` request/read model.                                  *)
(*                                                                         *)
(* The tool normalizes exact entity names, rejects blank/oversized lists,   *)
(* opens active entities only, returns active observations only, and        *)
(* surfaces only relations whose endpoints are both active.                 *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

RequestModes == {"valid_dupe_trim", "empty", "blank", "too_many"}
Outcomes == {"ok", "rejected"}
Reasons == {"none", "empty_names", "blank_name", "too_many_names"}
Names == {"active", "neighbor", "deleted_out", "deleted_in", "missing"}

Entities ==
    { [name |-> "active", active |-> TRUE],
      [name |-> "neighbor", active |-> TRUE],
      [name |-> "deleted_out", active |-> FALSE],
      [name |-> "deleted_in", active |-> FALSE] }

Observations ==
    { [entity |-> "active", content |-> "alpha", active |-> TRUE],
      [entity |-> "active", content |-> "old-alpha", active |-> FALSE],
      [entity |-> "deleted_out", content |-> "deleted", active |-> TRUE] }

Relations ==
    { [from |-> "active", to |-> "neighbor", relation_type |-> "related_to",
       active |-> TRUE],
      [from |-> "active", to |-> "deleted_out", relation_type |-> "related_to",
       active |-> TRUE],
      [from |-> "deleted_in", to |-> "active", relation_type |-> "related_to",
       active |-> TRUE],
      [from |-> "neighbor", to |-> "active", relation_type |-> "old",
       active |-> FALSE] }

Requests ==
    { [id |-> 1, mode |-> "valid_dupe_trim"],
      [id |-> 2, mode |-> "empty"],
      [id |-> 3, mode |-> "blank"],
      [id |-> 4, mode |-> "too_many"] }

RequestIds == {r.id : r \in Requests}

RawCountFor(r) ==
    CASE r.mode = "too_many" -> 101
      [] r.mode = "empty" -> 0
      [] OTHER -> 2

NormalizedNamesFor(r) ==
    CASE r.mode = "valid_dupe_trim" -> {"active"}
      [] OTHER -> {}

ReasonFor(r) ==
    CASE r.mode = "empty" -> "empty_names"
      [] r.mode = "blank" -> "blank_name"
      [] r.mode = "too_many" -> "too_many_names"
      [] OTHER -> "none"

ActiveEntityNames ==
    {e.name : e \in {x \in Entities : x.active}}

OpenedNamesFor(r) ==
    IF ReasonFor(r) # "none" THEN {}
    ELSE NormalizedNamesFor(r) \cap ActiveEntityNames

ActiveObservationRowsFor(name) ==
    {o.content : o \in {x \in Observations : x.entity = name /\ x.active}}

RelationEndpointsActive(rel) ==
    /\ rel.from \in ActiveEntityNames
    /\ rel.to \in ActiveEntityNames

RelationsOutFor(name) ==
    {rel.to : rel \in {x \in Relations :
        /\ x.from = name
        /\ x.active
        /\ RelationEndpointsActive(x)}}

RelationsInFor(name) ==
    {rel.from : rel \in {x \in Relations :
        /\ x.to = name
        /\ x.active
        /\ RelationEndpointsActive(x)}}

ResponseFor(r) ==
    LET reason == ReasonFor(r) IN
    LET ok == reason = "none" IN
        [ request_id |-> r.id,
          outcome |-> IF ok THEN "ok" ELSE "rejected",
          reason |-> reason,
          raw_count |-> RawCountFor(r),
          requested_names |-> IF ok THEN NormalizedNamesFor(r) ELSE {},
          name_cap |-> 100,
          opened_names |-> IF ok THEN OpenedNamesFor(r) ELSE {},
          observations_active |-> IF ok THEN ActiveObservationRowsFor("active") ELSE {},
          relations_out_active |-> IF ok THEN RelationsOutFor("active") ELSE {},
          relations_in_active |-> IF ok THEN RelationsInFor("active") ELSE {},
          writes |-> 0,
          lock_held |-> FALSE ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      reason: Reasons,
      raw_count: 0..101,
      requested_names: SUBSET Names,
      name_cap: 100..100,
      opened_names: SUBSET Names,
      observations_active: SUBSET {"alpha", "old-alpha", "deleted"},
      relations_out_active: SUBSET Names,
      relations_in_active: SUBSET Names,
      writes: 0..0,
      lock_held: BOOLEAN ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK == response \in ResponseRecord

InvalidNamesReject ==
    ReasonFor(req) # "none" =>
        /\ response.outcome = "rejected"
        /\ response.opened_names = {}

NameListBounded ==
    response.outcome = "ok" =>
        /\ response.raw_count <= response.name_cap
        /\ Cardinality(response.requested_names) <= response.name_cap

NamesNormalizedAndDeduped ==
    response.outcome = "ok" /\ req.mode = "valid_dupe_trim" =>
        response.requested_names = {"active"}

ActiveEntitiesOnly ==
    response.outcome = "ok" =>
        response.opened_names \subseteq ActiveEntityNames

ActiveObservationsOnly ==
    response.outcome = "ok" =>
        /\ "alpha" \in response.observations_active
        /\ "old-alpha" \notin response.observations_active

ActiveRelationEndpointsOnly ==
    response.outcome = "ok" =>
        /\ response.relations_out_active = {"neighbor"}
        /\ response.relations_in_active = {}
        /\ "deleted_out" \notin response.relations_out_active
        /\ "deleted_in" \notin response.relations_in_active

ReadOnlyNoHeldLock ==
    /\ response.writes = 0
    /\ response.lock_held = FALSE

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidNamesReject /\
        NameListBounded /\
        NamesNormalizedAndDeduped /\
        ActiveEntitiesOnly /\
        ActiveObservationsOnly /\
        ActiveRelationEndpointsOnly /\
        ReadOnlyNoHeldLock)

================================================================================
