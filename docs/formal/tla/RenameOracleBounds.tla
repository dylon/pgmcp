------------------------------ MODULE RenameOracleBounds ------------------------------
(***************************************************************************)
(* `rename_oracle` request boundary.                                       *)
(*                                                                         *)
(* The tool trims and validates the removed symbol and current-name set,    *)
(* bounds the dictionary input before constructing a local DAWG/transducer, *)
(* deduplicates candidates, and streams the best edit-distance candidate    *)
(* without collecting every match.                                          *)
(*                                                                         *)
(* Verified:                                                               *)
(*   - invalid names reject before dictionary construction;                 *)
(*   - candidate count, per-name bytes, and total bytes are bounded;        *)
(*   - empty candidate sets return no match without dictionary work;        *)
(*   - duplicate candidates are collapsed before the dictionary boundary;   *)
(*   - best-candidate selection keeps at most one candidate in memory.      *)
(***************************************************************************)

EXTENDS Naturals

MaxCandidates == 5000
MaxNameBytes == 256
MaxTotalBytes == 1048576
MaxDistance == 2

Reasons ==
    {"none", "blank_removed", "removed_too_long", "too_many_candidates",
     "blank_candidate", "candidate_too_long", "total_too_large"}

NoReq ==
    [ id |-> 0,
      removed_blank |-> FALSE,
      removed_bytes |-> 1,
      raw_count |-> 0,
      blank_candidate |-> FALSE,
      max_candidate_bytes |-> 0,
      total_bytes |-> 1,
      distinct_count |-> 0,
      match_exists |-> FALSE ]

Requests ==
    { [ id |-> 1, removed_blank |-> FALSE, removed_bytes |-> 12,
        raw_count |-> 4, blank_candidate |-> FALSE, max_candidate_bytes |-> 13,
        total_bytes |-> 50, distinct_count |-> 3, match_exists |-> TRUE ],
      [ id |-> 2, removed_blank |-> TRUE, removed_bytes |-> 0,
        raw_count |-> 1, blank_candidate |-> FALSE, max_candidate_bytes |-> 12,
        total_bytes |-> 12, distinct_count |-> 1, match_exists |-> TRUE ],
      [ id |-> 3, removed_blank |-> FALSE, removed_bytes |-> 257,
        raw_count |-> 1, blank_candidate |-> FALSE, max_candidate_bytes |-> 12,
        total_bytes |-> 269, distinct_count |-> 1, match_exists |-> TRUE ],
      [ id |-> 4, removed_blank |-> FALSE, removed_bytes |-> 12,
        raw_count |-> 5001, blank_candidate |-> FALSE, max_candidate_bytes |-> 12,
        total_bytes |-> 60012, distinct_count |-> 5001, match_exists |-> TRUE ],
      [ id |-> 5, removed_blank |-> FALSE, removed_bytes |-> 12,
        raw_count |-> 1, blank_candidate |-> TRUE, max_candidate_bytes |-> 0,
        total_bytes |-> 12, distinct_count |-> 0, match_exists |-> FALSE ],
      [ id |-> 6, removed_blank |-> FALSE, removed_bytes |-> 12,
        raw_count |-> 1, blank_candidate |-> FALSE, max_candidate_bytes |-> 257,
        total_bytes |-> 269, distinct_count |-> 1, match_exists |-> TRUE ],
      [ id |-> 7, removed_blank |-> FALSE, removed_bytes |-> 12,
        raw_count |-> 2, blank_candidate |-> FALSE, max_candidate_bytes |-> 200,
        total_bytes |-> 1048577, distinct_count |-> 2, match_exists |-> TRUE ],
      [ id |-> 8, removed_blank |-> FALSE, removed_bytes |-> 12,
        raw_count |-> 0, blank_candidate |-> FALSE, max_candidate_bytes |-> 0,
        total_bytes |-> 12, distinct_count |-> 0, match_exists |-> FALSE ],
      [ id |-> 9, removed_blank |-> FALSE, removed_bytes |-> 12,
        raw_count |-> 2, blank_candidate |-> FALSE, max_candidate_bytes |-> 14,
        total_bytes |-> 40, distinct_count |-> 2, match_exists |-> FALSE ] }

Reject(reason) ==
    [ rejected |-> TRUE,
      reason |-> reason,
      normalized |-> FALSE,
      dictionary_builds |-> 0,
      candidate_count |-> 0,
      best_buffer_size |-> 0,
      max_distance |-> MaxDistance,
      likely_match |-> FALSE ]

Accept(r) ==
    [ rejected |-> FALSE,
      reason |-> "none",
      normalized |-> TRUE,
      dictionary_builds |-> IF r.distinct_count = 0 THEN 0 ELSE 1,
      candidate_count |-> r.distinct_count,
      best_buffer_size |-> IF r.distinct_count = 0 \/ ~r.match_exists THEN 0 ELSE 1,
      max_distance |-> MaxDistance,
      likely_match |-> r.match_exists ]

Evaluate(r) ==
    IF r.removed_blank THEN Reject("blank_removed")
    ELSE IF r.removed_bytes > MaxNameBytes THEN Reject("removed_too_long")
    ELSE IF r.raw_count > MaxCandidates THEN Reject("too_many_candidates")
    ELSE IF r.blank_candidate THEN Reject("blank_candidate")
    ELSE IF r.max_candidate_bytes > MaxNameBytes THEN Reject("candidate_too_long")
    ELSE IF r.total_bytes > MaxTotalBytes THEN Reject("total_too_large")
    ELSE Accept(r)

NoResp ==
    [ rejected |-> FALSE,
      reason |-> "none",
      normalized |-> FALSE,
      dictionary_builds |-> 0,
      candidate_count |-> 0,
      best_buffer_size |-> 0,
      max_distance |-> MaxDistance,
      likely_match |-> FALSE ]

VARIABLES req, resp

vars == <<req, resp>>

Init ==
    /\ req = NoReq
    /\ resp = NoResp

Handle(r) ==
    /\ req = NoReq
    /\ r \in Requests
    /\ req' = r
    /\ resp' = Evaluate(r)

Done ==
    /\ req # NoReq
    /\ UNCHANGED vars

Next ==
    \/ \E r \in Requests : Handle(r)
    \/ Done

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ resp.rejected \in BOOLEAN
    /\ resp.reason \in Reasons
    /\ resp.normalized \in BOOLEAN
    /\ resp.dictionary_builds \in 0..1
    /\ resp.candidate_count \in 0..MaxCandidates
    /\ resp.best_buffer_size \in 0..1
    /\ resp.max_distance = MaxDistance
    /\ resp.likely_match \in BOOLEAN

InvalidInputsDoNotBuildDictionary ==
    req # NoReq /\ resp.reason # "none" =>
        /\ resp.dictionary_builds = 0
        /\ resp.best_buffer_size = 0
        /\ resp.likely_match = FALSE

CandidateCountBounded ==
    req # NoReq => resp.candidate_count <= MaxCandidates

EmptyCandidatesAvoidDictionary ==
    req # NoReq /\ ~resp.rejected /\ req.distinct_count = 0 =>
        /\ resp.dictionary_builds = 0
        /\ resp.candidate_count = 0
        /\ resp.likely_match = FALSE

DedupedCountUsed ==
    req # NoReq /\ ~resp.rejected =>
        resp.candidate_count = req.distinct_count

StreamingBestSelection ==
    req # NoReq => resp.best_buffer_size <= 1

NormalizedSuccess ==
    req # NoReq /\ ~resp.rejected => resp.normalized

=============================================================================
