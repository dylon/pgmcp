---------------------------- MODULE MemoryAddObservations ----------------------------
(***************************************************************************)
(* `memory_add_observations` append boundary.                             *)
(*                                                                         *)
(* The official-compatible request identifies an entity by name. pgmcp's   *)
(* local safety obligation is to append only when that name resolves to a   *)
(* unique active entity, dedupe content per entity, and preserve            *)
(* agent-write provenance for inserted observations.                       *)
(***************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

Entities ==
    { [id |-> 1, name |-> "unique", active |-> TRUE],
      [id |-> 2, name |-> "duplicate", active |-> TRUE],
      [id |-> 3, name |-> "duplicate", active |-> TRUE],
      [id |-> 4, name |-> "expired", active |-> FALSE] }

Contents == {"hello", "world", "new"}
Sources == {"agent_write"}

NoReq == [id |-> 0, entity_name |-> "", contents |-> {}]

Requests ==
    { [id |-> 1, entity_name |-> "unique", contents |-> {"hello", "world"}],
      [id |-> 2, entity_name |-> "duplicate", contents |-> {"new"}],
      [id |-> 3, entity_name |-> "missing", contents |-> {"new"}],
      [id |-> 4, entity_name |-> "expired", contents |-> {"new"}] }

RequestIds == {r.id : r \in Requests}
EntityIds == {e.id : e \in Entities}
Outcomes == {"ok", "rejected"}

ActiveMatches(name) == {e \in Entities : e.name = name /\ e.active}
ResolvedEntityId(r) ==
    IF Cardinality(ActiveMatches(r.entity_name)) = 1
    THEN (CHOOSE e \in ActiveMatches(r.entity_name) : TRUE).id
    ELSE 0

RequestFor(id) == CHOOSE r \in Requests : r.id = id

ObservationRecord ==
    [ entity_id: EntityIds,
      content: Contents,
      source: Sources ]

VARIABLES phase, req, observations, responses, seen

vars == <<phase, req, observations, responses, seen>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      resolved_entity_id: EntityIds \cup {0},
      inserted: SUBSET ObservationRecord ]

Init ==
    /\ phase = "idle"
    /\ req = NoReq
    /\ observations = {[entity_id |-> 1, content |-> "hello", source |-> "agent_write"]}
    /\ responses = <<>>
    /\ seen = {}

PickRequest(r) ==
    /\ phase = "idle"
    /\ r \in Requests
    /\ r.id \notin seen
    /\ req' = r
    /\ phase' = "pending"
    /\ UNCHANGED <<observations, responses, seen>>

RejectAmbiguous ==
    /\ phase = "pending"
    /\ Cardinality(ActiveMatches(req.entity_name)) > 1
    /\ observations' = observations
    /\ responses' =
        Append(responses,
            [ request_id |-> req.id,
              outcome |-> "rejected",
              resolved_entity_id |-> 0,
              inserted |-> {} ])
    /\ seen' = seen \cup {req.id}
    /\ phase' = "done"
    /\ UNCHANGED req

AppendForUniqueOrMissing ==
    /\ phase = "pending"
    /\ Cardinality(ActiveMatches(req.entity_name)) <= 1
    /\ LET eid == ResolvedEntityId(req) IN
       LET candidates ==
            IF eid = 0 THEN {}
            ELSE {[entity_id |-> eid, content |-> c, source |-> "agent_write"] : c \in req.contents} IN
       LET inserted == candidates \ observations IN
       /\ observations' = observations \cup inserted
       /\ responses' =
            Append(responses,
                [ request_id |-> req.id,
                  outcome |-> "ok",
                  resolved_entity_id |-> eid,
                  inserted |-> inserted ])
    /\ seen' = seen \cup {req.id}
    /\ phase' = "done"
    /\ UNCHANGED req

Reset ==
    /\ phase = "done"
    /\ req' = NoReq
    /\ phase' = "idle"
    /\ UNCHANGED <<observations, responses, seen>>

Next ==
    \/ \E r \in Requests : PickRequest(r)
    \/ RejectAmbiguous
    \/ AppendForUniqueOrMissing
    \/ Reset

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ phase \in {"idle", "pending", "done"}
    /\ req \in Requests \cup {NoReq}
    /\ observations \subseteq ObservationRecord
    /\ responses \in Seq(ResponseRecord)
    /\ seen \subseteq RequestIds

AmbiguousNamesRejectedNoWrite ==
    \A i \in 1..Len(responses) :
        LET r == RequestFor(responses[i].request_id) IN
        Cardinality(ActiveMatches(r.entity_name)) > 1 =>
            /\ responses[i].outcome = "rejected"
            /\ responses[i].inserted = {}

MissingOrExpiredNoWrite ==
    \A i \in 1..Len(responses) :
        LET r == RequestFor(responses[i].request_id) IN
        Cardinality(ActiveMatches(r.entity_name)) = 0 =>
            responses[i].inserted = {}

InsertedRowsBelongToResolvedEntity ==
    \A i \in 1..Len(responses) :
        \A obs \in responses[i].inserted :
            obs.entity_id = responses[i].resolved_entity_id

InsertedRowsAreAgentWrite ==
    \A i \in 1..Len(responses) :
        \A obs \in responses[i].inserted : obs.source = "agent_write"

NoDuplicateEntityContent ==
    \A o1, o2 \in observations :
        (o1.entity_id = o2.entity_id /\ o1.content = o2.content) => o1 = o2

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        AmbiguousNamesRejectedNoWrite /\
        MissingOrExpiredNoWrite /\
        InsertedRowsBelongToResolvedEntity /\
        InsertedRowsAreAgentWrite /\
        NoDuplicateEntityContent)

=============================================================================
