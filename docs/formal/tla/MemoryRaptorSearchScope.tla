-------------------------- MODULE MemoryRaptorSearchScope --------------------------
(***************************************************************************)
(* `memory_raptor_search` request boundary and per-level retrieval model.   *)
(*                                                                         *)
(* The tool rejects malformed requests before embedding/querying, normalizes *)
(* level filters, clamps k/ef_search, and the SQL helper returns up to k     *)
(* nearest RAPTOR summary nodes per requested level rather than one global   *)
(* limit that can starve higher abstraction levels.                         *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

ExpectedDim == 1024
MaxK == 200
MaxEf == 10000
MaxLevelEntries == 16
MaxLevel == 32

Levels == 0..2
LevelModes == {"none", "valid", "empty", "duplicate", "negative", "too_many", "out_of_range"}
Outcomes == {"ok", "rejected"}
Reasons ==
    {"none", "blank_query", "bad_scope", "bad_embedding_dim",
     "empty_levels", "too_many_levels", "bad_level"}

Requests ==
    { [ id |-> 1, raw_query |-> " topic ", scope_id |-> 1,
        embed_dim |-> ExpectedDim, level_mode |-> "none", k |-> 999, ef |-> -5 ],
      [ id |-> 2, raw_query |-> "   ", scope_id |-> 1,
        embed_dim |-> ExpectedDim, level_mode |-> "none", k |-> 10, ef |-> 64 ],
      [ id |-> 3, raw_query |-> "topic", scope_id |-> -1,
        embed_dim |-> ExpectedDim, level_mode |-> "none", k |-> 10, ef |-> 64 ],
      [ id |-> 4, raw_query |-> "topic", scope_id |-> 1,
        embed_dim |-> 384, level_mode |-> "none", k |-> 10, ef |-> 64 ],
      [ id |-> 5, raw_query |-> "topic", scope_id |-> 1,
        embed_dim |-> ExpectedDim, level_mode |-> "empty", k |-> 10, ef |-> 64 ],
      [ id |-> 6, raw_query |-> "topic", scope_id |-> 1,
        embed_dim |-> ExpectedDim, level_mode |-> "negative", k |-> 10, ef |-> 64 ],
      [ id |-> 7, raw_query |-> "topic", scope_id |-> 1,
        embed_dim |-> ExpectedDim, level_mode |-> "too_many", k |-> 10, ef |-> 64 ],
      [ id |-> 8, raw_query |-> "topic", scope_id |-> 1,
        embed_dim |-> ExpectedDim, level_mode |-> "out_of_range", k |-> 10, ef |-> 64 ],
      [ id |-> 9, raw_query |-> "topic", scope_id |-> 1,
        embed_dim |-> ExpectedDim, level_mode |-> "valid", k |-> 1, ef |-> 64 ],
      [ id |-> 10, raw_query |-> "topic", scope_id |-> 1,
        embed_dim |-> ExpectedDim, level_mode |-> "duplicate", k |-> 1, ef |-> 64 ] }

RequestIds == {r.id : r \in Requests}

NormalizeQuery(raw) ==
    CASE raw = " topic " -> "topic"
      [] raw = "   " -> ""
      [] OTHER -> raw

RawLevelCount(mode) ==
    CASE mode = "none" -> 0
      [] mode = "valid" -> 2
      [] mode = "empty" -> 0
      [] mode = "duplicate" -> 2
      [] mode = "negative" -> 1
      [] mode = "too_many" -> MaxLevelEntries + 1
      [] mode = "out_of_range" -> 1

NormalizedLevels(mode) ==
    CASE mode = "none" -> Levels
      [] mode = "valid" -> {0, 1}
      [] mode = "duplicate" -> {1}
      [] OTHER -> {}

KFor(r) ==
    IF r.k < 1 THEN 1
    ELSE IF r.k > MaxK THEN MaxK
    ELSE r.k

EfFor(r) ==
    IF r.ef < 1 THEN 1
    ELSE IF r.ef > MaxEf THEN MaxEf
    ELSE r.ef

Available(level) ==
    CASE level = 0 -> 2
      [] level = 1 -> 2
      [] level = 2 -> 1

Min(a, b) == IF a <= b THEN a ELSE b

ReasonFor(r) ==
    CASE NormalizeQuery(r.raw_query) = "" -> "blank_query"
      [] r.scope_id <= 0 -> "bad_scope"
      [] r.embed_dim # ExpectedDim -> "bad_embedding_dim"
      [] r.level_mode = "empty" -> "empty_levels"
      [] RawLevelCount(r.level_mode) > MaxLevelEntries -> "too_many_levels"
      [] r.level_mode \in {"negative", "out_of_range"} -> "bad_level"
      [] OTHER -> "none"

LevelCountsFor(r) ==
    [level \in Levels |->
        IF ReasonFor(r) = "none" /\ level \in NormalizedLevels(r.level_mode)
        THEN Min(Available(level), KFor(r))
        ELSE 0]

ResponseFor(r) ==
    LET reason == ReasonFor(r) IN
    LET counts == LevelCountsFor(r) IN
        [ request_id |-> r.id,
          outcome |-> IF reason = "none" THEN "ok" ELSE "rejected",
          reason |-> reason,
          query |-> NormalizeQuery(r.raw_query),
          k |-> IF reason = "none" THEN KFor(r) ELSE 0,
          ef |-> IF reason = "none" THEN EfFor(r) ELSE 0,
          normalized_levels |-> IF reason = "none" THEN NormalizedLevels(r.level_mode) ELSE {},
          result_levels |-> {l \in Levels : counts[l] > 0},
          level_counts |-> counts,
          writes |-> 0,
          lock_held |-> FALSE ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      reason: Reasons,
      query: {"", "topic"},
      k: 0..MaxK,
      ef: 0..MaxEf,
      normalized_levels: SUBSET Levels,
      result_levels: SUBSET Levels,
      level_counts: [Levels -> 0..MaxK],
      writes: 0..0,
      lock_held: BOOLEAN ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK == response \in ResponseRecord

InvalidRequestsReject ==
    ReasonFor(req) # "none" => response.outcome = "rejected"

SuccessfulRequestShape ==
    response.outcome = "ok" =>
        /\ req.embed_dim = ExpectedDim
        /\ NormalizeQuery(req.raw_query) # ""
        /\ req.scope_id > 0
        /\ response.k \in 1..MaxK
        /\ response.ef \in 1..MaxEf

LevelsNormalized ==
    response.outcome = "ok" =>
        /\ response.normalized_levels = NormalizedLevels(req.level_mode)
        /\ response.normalized_levels # {}
        /\ Cardinality(response.normalized_levels) <= MaxLevelEntries
        /\ \A level \in response.normalized_levels : level \in 0..MaxLevel

ResultsOnlyRequestedLevels ==
    response.outcome = "ok" =>
        response.result_levels \subseteq response.normalized_levels

PerLevelTopKBound ==
    response.outcome = "ok" =>
        \A level \in Levels : response.level_counts[level] <= response.k

NoRequestedLevelStarvation ==
    response.outcome = "ok" =>
        \A level \in response.normalized_levels :
            Available(level) > 0 => response.level_counts[level] > 0

ReadOnlyNoHeldLock ==
    /\ response.writes = 0
    /\ response.lock_held = FALSE

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidRequestsReject /\
        SuccessfulRequestShape /\
        LevelsNormalized /\
        ResultsOnlyRequestedLevels /\
        PerLevelTopKBound /\
        NoRequestedLevelStarvation /\
        ReadOnlyNoHeldLock)

=============================================================================
