---------------------------- MODULE MemorySearchNodesScope ----------------------------
(***************************************************************************)
(* `memory_search_nodes` SQL boundary.                                    *)
(*                                                                         *)
(* The tool is a substring search over active memory entities and active    *)
(* observations. Correctness depends on treating LIKE metacharacters as     *)
(* literal query text, scope-filtering without multiplying rows, and        *)
(* counting matching observations exactly once per active observation.      *)
(***************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

Scopes == {"scope-a", "scope-b"}
Queries == {"percent", "needle", "control"}

Entities ==
    { [id |-> 1, name |-> "literal-percent", entity_type |-> "concept", active |-> TRUE, importance |-> 1],
      [id |-> 2, name |-> "wildcard-control", entity_type |-> "concept", active |-> TRUE, importance |-> 1],
      [id |-> 3, name |-> "scope-count-entity", entity_type |-> "concept", active |-> TRUE, importance |-> 1],
      [id |-> 4, name |-> "expired-needle", entity_type |-> "concept", active |-> FALSE, importance |-> 1] }

Observations ==
    { [entity_id |-> 1, content |-> "literal 100%_done", active |-> TRUE],
      [entity_id |-> 2, content |-> "plain control text", active |-> TRUE],
      [entity_id |-> 3, content |-> "one needle observation", active |-> TRUE],
      [entity_id |-> 3, content |-> "expired needle observation", active |-> FALSE],
      [entity_id |-> 4, content |-> "needle on expired entity", active |-> TRUE] }

Memberships ==
    { [entity_id |-> 1, scope |-> "scope-a"],
      [entity_id |-> 2, scope |-> "scope-a"],
      [entity_id |-> 3, scope |-> "scope-a"],
      [entity_id |-> 3, scope |-> "scope-b"] }

NoReq == [id |-> 0, query |-> "control", scope |-> "none", limit |-> 20]

Requests ==
    { [id |-> 1, query |-> "percent", scope |-> "scope-a", limit |-> 10],
      [id |-> 2, query |-> "needle", scope |-> "none", limit |-> 10],
      [id |-> 3, query |-> "needle", scope |-> "scope-b", limit |-> 0],
      [id |-> 4, query |-> "control", scope |-> "scope-b", limit |-> 500] }

RequestIds == {r.id : r \in Requests}
EntityIds == {e.id : e \in Entities}

ClampLimit(limit) ==
    IF limit < 1 THEN 1 ELSE IF limit > 500 THEN 500 ELSE limit

LiteralMatches(query, text) ==
    CASE query = "percent" -> text = "literal-percent" \/ text = "literal 100%_done"
      [] query = "needle" -> text = "scope-count-entity" \/ text = "one needle observation" \/ text = "expired needle observation" \/ text = "expired-needle" \/ text = "needle on expired entity"
      [] query = "control" -> text = "wildcard-control" \/ text = "plain control text"
      [] OTHER -> FALSE

EntityById(id) == CHOOSE e \in Entities : e.id = id

ScopeAllows(r, e) ==
    r.scope = "none" \/ \E m \in Memberships : m.entity_id = e.id /\ m.scope = r.scope

MatchingObservations(r, e) ==
    {o \in Observations :
        /\ o.entity_id = e.id
        /\ o.active
        /\ LiteralMatches(r.query, o.content)}

EntityMatches(r, e) ==
    \/ LiteralMatches(r.query, e.name)
    \/ LiteralMatches(r.query, e.entity_type)
    \/ MatchingObservations(r, e) # {}

VisibleEntities(r) ==
    {e \in Entities :
        /\ e.active
        /\ ScopeAllows(r, e)
        /\ EntityMatches(r, e)}

RequestFor(id) == CHOOSE r \in Requests : r.id = id

VARIABLES phase, req, responses, seen

vars == <<phase, req, responses, seen>>

RowRecord ==
    [ entity_id: EntityIds,
      matched_observations: 0..Cardinality(Observations) ]

ResponseRecord ==
    [ request_id: RequestIds,
      query: Queries,
      scope: Scopes \cup {"none"},
      effective_limit: 1..500,
      rows: SUBSET RowRecord ]

RowsFor(r) ==
    {[ entity_id |-> e.id,
       matched_observations |-> Cardinality(MatchingObservations(r, e))] :
        e \in VisibleEntities(r)}

Init ==
    /\ phase = "idle"
    /\ req = NoReq
    /\ responses = <<>>
    /\ seen = {}

PickRequest(r) ==
    /\ phase = "idle"
    /\ r \in Requests
    /\ r.id \notin seen
    /\ req' = r
    /\ phase' = "pending"
    /\ UNCHANGED <<responses, seen>>

ReturnRows ==
    /\ phase = "pending"
    /\ LET cap == ClampLimit(req.limit) IN
       \E rows \in SUBSET RowsFor(req) :
          /\ Cardinality(rows) <= cap
          /\ responses' =
              Append(responses,
                  [ request_id |-> req.id,
                    query |-> req.query,
                    scope |-> req.scope,
                    effective_limit |-> cap,
                    rows |-> rows ])
    /\ seen' = seen \cup {req.id}
    /\ phase' = "done"
    /\ UNCHANGED req

Reset ==
    /\ phase = "done"
    /\ req' = NoReq
    /\ phase' = "idle"
    /\ UNCHANGED <<responses, seen>>

Next ==
    \/ \E r \in Requests : PickRequest(r)
    \/ ReturnRows
    \/ Reset

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ phase \in {"idle", "pending", "done"}
    /\ req \in Requests \cup {NoReq}
    /\ responses \in Seq(ResponseRecord)
    /\ seen \subseteq RequestIds

RowsAreActiveAndScoped ==
    \A i \in 1..Len(responses) :
        LET r == RequestFor(responses[i].request_id) IN
        \A row \in responses[i].rows :
            LET e == EntityById(row.entity_id) IN
                /\ e.active
                /\ ScopeAllows(r, e)
                /\ EntityMatches(r, e)

MatchedObservationCountExact ==
    \A i \in 1..Len(responses) :
        LET r == RequestFor(responses[i].request_id) IN
        \A row \in responses[i].rows :
            row.matched_observations = Cardinality(MatchingObservations(r, EntityById(row.entity_id)))

WildcardQueryIsLiteral ==
    \A i \in 1..Len(responses) :
        responses[i].query = "percent" =>
            \A row \in responses[i].rows : row.entity_id = 1

OutputWithinLimit ==
    \A i \in 1..Len(responses) :
        Cardinality(responses[i].rows) <= responses[i].effective_limit

EffectiveLimitClamped ==
    \A i \in 1..Len(responses) :
        responses[i].effective_limit = ClampLimit(RequestFor(responses[i].request_id).limit)

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        RowsAreActiveAndScoped /\
        MatchedObservationCountExact /\
        WildcardQueryIsLiteral /\
        OutputWithinLimit /\
        EffectiveLimitClamped)

=============================================================================
