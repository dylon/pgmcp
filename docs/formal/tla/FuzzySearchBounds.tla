------------------------------ MODULE FuzzySearchBounds ------------------------------
(***************************************************************************)
(* pgmcp fuzzy-search MCP boundary.                                        *)
(*                                                                         *)
(* The trie/transducer implementations are verified in sibling libraries.   *)
(* pgmcp's local obligation is to normalize caller request bounds before    *)
(* invoking them, and to preserve per-project vocabulary isolation in the   *)
(* response.                                                               *)
(***************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

Tools == {"symbol", "path", "phonetic_symbol"}
Projects == {"alpha", "beta"}

OverCapDistance == 1000
DefaultDistance == 2
MaxDistance == 64
DefaultLimit == 20
MaxLimit == 100

NoReq == [id |-> 0, tool |-> "symbol", project |-> "alpha", max_distance |-> DefaultDistance, limit |-> DefaultLimit]

Requests ==
    { [id |-> 1, tool |-> "symbol", project |-> "alpha", max_distance |-> OverCapDistance, limit |-> 0],
      [id |-> 2, tool |-> "path", project |-> "alpha", max_distance |-> 2, limit |-> 500],
      [id |-> 3, tool |-> "phonetic_symbol", project |-> "beta", max_distance |-> 2, limit |-> 20],
      [id |-> 4, tool |-> "symbol", project |-> "alpha", max_distance |-> 0, limit |-> 20] }

RequestIds == {r.id : r \in Requests}

Vocabulary ==
    { [project |-> "alpha", key |-> "FcmBackend", raw_distance |-> 1, phonetic_distance |-> 1],
      [project |-> "alpha", key |-> "FcmBakend", raw_distance |-> 0, phonetic_distance |-> 0],
      [project |-> "alpha", key |-> "far_away_symbol", raw_distance |-> 80, phonetic_distance |-> 80],
      [project |-> "beta", key |-> "BetaBackend", raw_distance |-> 1, phonetic_distance |-> 1] }

ClampDistance(distance) ==
    IF distance > MaxDistance THEN MaxDistance ELSE distance

ClampLimit(limit) ==
    IF limit < 1 THEN 1 ELSE IF limit > MaxLimit THEN MaxLimit ELSE limit

DistanceFor(r, row) ==
    IF r.tool = "phonetic_symbol" THEN row.phonetic_distance ELSE row.raw_distance

VisibleRows(r) ==
    {row \in Vocabulary :
        /\ row.project = r.project
        /\ DistanceFor(r, row) <= ClampDistance(r.max_distance)}

RequestFor(id) == CHOOSE r \in Requests : r.id = id

VARIABLES phase, req, responses, seen

vars == <<phase, req, responses, seen>>

ResponseRecord ==
    [ request_id: RequestIds,
      tool: Tools,
      project: Projects,
      effective_distance: 0..MaxDistance,
      effective_limit: 1..MaxLimit,
      rows: SUBSET Vocabulary ]

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
    /\ LET effective_distance == ClampDistance(req.max_distance) IN
       LET effective_limit == ClampLimit(req.limit) IN
       \E rows \in SUBSET VisibleRows(req) :
          /\ Cardinality(rows) <= effective_limit
          /\ responses' =
              Append(responses,
                  [ request_id |-> req.id,
                    tool |-> req.tool,
                    project |-> req.project,
                    effective_distance |-> effective_distance,
                    effective_limit |-> effective_limit,
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

EffectiveDistanceClamped ==
    \A i \in 1..Len(responses) :
        responses[i].effective_distance =
            ClampDistance(RequestFor(responses[i].request_id).max_distance)

EffectiveLimitClamped ==
    \A i \in 1..Len(responses) :
        responses[i].effective_limit =
            ClampLimit(RequestFor(responses[i].request_id).limit)

RowsProjectScoped ==
    \A i \in 1..Len(responses) :
        \A row \in responses[i].rows :
            row.project = responses[i].project

RowsWithinEffectiveDistance ==
    \A i \in 1..Len(responses) :
        LET r == RequestFor(responses[i].request_id) IN
        \A row \in responses[i].rows :
            DistanceFor(r, row) <= responses[i].effective_distance

OutputWithinLimit ==
    \A i \in 1..Len(responses) :
        Cardinality(responses[i].rows) <= responses[i].effective_limit

ExactModeDoesNotAdmitTypos ==
    \A i \in 1..Len(responses) :
        LET r == RequestFor(responses[i].request_id) IN
        r.max_distance = 0 =>
            \A row \in responses[i].rows : DistanceFor(r, row) = 0

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        EffectiveDistanceClamped /\
        EffectiveLimitClamped /\
        RowsProjectScoped /\
        RowsWithinEffectiveDistance /\
        OutputWithinLimit /\
        ExactModeDoesNotAdmitTypos)

=============================================================================
