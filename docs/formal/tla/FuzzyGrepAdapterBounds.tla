---------------------------- MODULE FuzzyGrepAdapterBounds ----------------------------
(***************************************************************************)
(* `fuzzy_grep` adapter boundary model.  The edit-distance and TokenGrep    *)
(* matching semantics are inherited from liblevenshtein/libdictenstein; this *)
(* spec checks pgmcp's caller-controlled request and output bounds.          *)
(***************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

MaxDistance == 8
MaxMatches == 3
MaxQueryBytes == 4
MaxDocs == 2

Outcomes == {"ok", "rejected"}
RequestIds == {1, 2, 3, 4, 5, 6}

Requests ==
    { [id |-> 1, query_ok |-> TRUE, query_bytes |-> 4, docs |-> 1,
       docs_ok |-> TRUE, total_ok |-> TRUE, raw_distance |-> 2,
       explicit_distance |-> 2, candidate_matches |-> 2],
      [id |-> 2, query_ok |-> FALSE, query_bytes |-> 0, docs |-> 1,
       docs_ok |-> TRUE, total_ok |-> TRUE, raw_distance |-> 2,
       explicit_distance |-> 2, candidate_matches |-> 1],
      [id |-> 3, query_ok |-> TRUE, query_bytes |-> 5, docs |-> 1,
       docs_ok |-> TRUE, total_ok |-> TRUE, raw_distance |-> 2,
       explicit_distance |-> 2, candidate_matches |-> 1],
      [id |-> 4, query_ok |-> TRUE, query_bytes |-> 4, docs |-> 3,
       docs_ok |-> TRUE, total_ok |-> TRUE, raw_distance |-> 2,
       explicit_distance |-> 2, candidate_matches |-> 1],
      [id |-> 5, query_ok |-> TRUE, query_bytes |-> 4, docs |-> 1,
       docs_ok |-> TRUE, total_ok |-> TRUE, raw_distance |-> 256,
       explicit_distance |-> 2, candidate_matches |-> 1],
      [id |-> 6, query_ok |-> TRUE, query_bytes |-> 4, docs |-> 1,
       docs_ok |-> TRUE, total_ok |-> TRUE, raw_distance |-> 2,
       explicit_distance |-> 9, candidate_matches |-> 4] }

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      scanned: BOOLEAN,
      max_distance: 0..MaxDistance,
      reported: 0..MaxMatches,
      truncated: BOOLEAN ]

VARIABLES responses, seen, dbState

vars == <<responses, seen, dbState>>

Init ==
    /\ responses = <<>>
    /\ seen = {}
    /\ dbState = "unchanged"

ValidRequest(r) ==
    /\ r.query_ok
    /\ r.query_bytes > 0
    /\ r.query_bytes <= MaxQueryBytes
    /\ r.docs <= MaxDocs
    /\ r.docs_ok
    /\ r.total_ok
    /\ r.explicit_distance <= MaxDistance

ClampDistance(d) ==
    IF d > MaxDistance THEN MaxDistance ELSE d

Reported(candidates) ==
    IF candidates > MaxMatches THEN MaxMatches ELSE candidates

Process(r) ==
    /\ r \in Requests
    /\ r.id \notin seen
    /\ seen' = seen \cup {r.id}
    /\ dbState' = dbState
    /\ IF ValidRequest(r) THEN
          /\ responses' = Append(responses,
                [request_id |-> r.id,
                 outcome |-> "ok",
                 scanned |-> TRUE,
                 max_distance |-> ClampDistance(r.raw_distance),
                 reported |-> Reported(r.candidate_matches),
                 truncated |-> r.candidate_matches > MaxMatches])
       ELSE
          /\ responses' = Append(responses,
                [request_id |-> r.id,
                 outcome |-> "rejected",
                 scanned |-> FALSE,
                 max_distance |-> 0,
                 reported |-> 0,
                 truncated |-> FALSE])

Next == \E r \in Requests : Process(r)

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ responses \in Seq(ResponseRecord)
    /\ seen \subseteq RequestIds
    /\ dbState = "unchanged"

InvalidRequestsDoNotScan ==
    \A i \in 1..Len(responses) :
        LET r == CHOOSE x \in Requests : x.id = responses[i].request_id IN
        ~ValidRequest(r) => responses[i].scanned = FALSE

ScansOnlyValidRequests ==
    \A i \in 1..Len(responses) :
        LET r == CHOOSE x \in Requests : x.id = responses[i].request_id IN
        responses[i].scanned => ValidRequest(r)

DistanceNeverWrapsOrExceedsCap ==
    \A i \in 1..Len(responses) :
        LET r == CHOOSE x \in Requests : x.id = responses[i].request_id IN
        responses[i].scanned =>
            /\ responses[i].max_distance <= MaxDistance
            /\ responses[i].max_distance = ClampDistance(r.raw_distance)

ReportedMatchesBounded ==
    \A i \in 1..Len(responses) :
        responses[i].reported <= MaxMatches

TruncationSound ==
    \A i \in 1..Len(responses) :
        LET r == CHOOSE x \in Requests : x.id = responses[i].request_id IN
        responses[i].scanned =>
            /\ responses[i].reported = Reported(r.candidate_matches)
            /\ responses[i].truncated = (r.candidate_matches > MaxMatches)

ReadOnlyAdapter ==
    dbState = "unchanged"

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidRequestsDoNotScan /\
        ScansOnlyValidRequests /\
        DistanceNeverWrapsOrExceedsCap /\
        ReportedMatchesBounded /\
        TruncationSound /\
        ReadOnlyAdapter)

================================================================================
