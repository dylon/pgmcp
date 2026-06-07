----------------------------- MODULE SubstringSearchBounds -----------------------------
(***************************************************************************)
(* `substring_search` exact in-memory adapter boundary.                    *)
(*                                                                         *)
(* The tool preserves exact, case-sensitive substring semantics while       *)
(* bounding caller-supplied needle/haystack data before constructing the    *)
(* suffix-automaton index. Duplicate haystack terms are removed first; an   *)
(* empty accepted haystack returns false without index construction.        *)
(*                                                                         *)
(* Verified:                                                               *)
(*   - empty or oversized needle/terms reject before index construction;    *)
(*   - haystack count and total bytes are bounded;                          *)
(*   - empty accepted haystacks avoid index construction;                   *)
(*   - reported haystack size is the deduped count;                         *)
(*   - the result is exact membership over the deduped haystack;            *)
(*   - execution is read-only.                                             *)
(***************************************************************************)

EXTENDS Naturals

MaxNeedleBytes == 4096
MaxHaystackTerms == 5000
MaxTermBytes == 4096
MaxTotalBytes == 1048576

Reasons ==
    {"none", "empty_needle", "needle_too_long", "too_many_terms",
     "empty_term", "term_too_long", "total_too_large"}

NoReq ==
    [ id |-> 0,
      needle_bytes |-> 1,
      needle_empty |-> FALSE,
      raw_count |-> 0,
      distinct_count |-> 0,
      empty_term |-> FALSE,
      max_term_bytes |-> 0,
      total_bytes |-> 1,
      contains |-> FALSE ]

Requests ==
    { [ id |-> 1, needle_bytes |-> 4, needle_empty |-> FALSE,
        raw_count |-> 3, distinct_count |-> 2, empty_term |-> FALSE,
        max_term_bytes |-> 9, total_bytes |-> 22, contains |-> TRUE ],
      [ id |-> 2, needle_bytes |-> 0, needle_empty |-> TRUE,
        raw_count |-> 1, distinct_count |-> 1, empty_term |-> FALSE,
        max_term_bytes |-> 5, total_bytes |-> 5, contains |-> TRUE ],
      [ id |-> 3, needle_bytes |-> 4097, needle_empty |-> FALSE,
        raw_count |-> 1, distinct_count |-> 1, empty_term |-> FALSE,
        max_term_bytes |-> 5, total_bytes |-> 4102, contains |-> TRUE ],
      [ id |-> 4, needle_bytes |-> 1, needle_empty |-> FALSE,
        raw_count |-> 5001, distinct_count |-> 5001, empty_term |-> FALSE,
        max_term_bytes |-> 8, total_bytes |-> 40009, contains |-> TRUE ],
      [ id |-> 5, needle_bytes |-> 1, needle_empty |-> FALSE,
        raw_count |-> 1, distinct_count |-> 0, empty_term |-> TRUE,
        max_term_bytes |-> 0, total_bytes |-> 1, contains |-> FALSE ],
      [ id |-> 6, needle_bytes |-> 1, needle_empty |-> FALSE,
        raw_count |-> 1, distinct_count |-> 1, empty_term |-> FALSE,
        max_term_bytes |-> 4097, total_bytes |-> 4098, contains |-> TRUE ],
      [ id |-> 7, needle_bytes |-> 1, needle_empty |-> FALSE,
        raw_count |-> 2, distinct_count |-> 2, empty_term |-> FALSE,
        max_term_bytes |-> 2048, total_bytes |-> 1048577, contains |-> TRUE ],
      [ id |-> 8, needle_bytes |-> 1, needle_empty |-> FALSE,
        raw_count |-> 0, distinct_count |-> 0, empty_term |-> FALSE,
        max_term_bytes |-> 0, total_bytes |-> 1, contains |-> FALSE ],
      [ id |-> 9, needle_bytes |-> 4, needle_empty |-> FALSE,
        raw_count |-> 1, distinct_count |-> 1, empty_term |-> FALSE,
        max_term_bytes |-> 9, total_bytes |-> 13, contains |-> FALSE ] }

Reject(reason) ==
    [ rejected |-> TRUE,
      reason |-> reason,
      index_builds |-> 0,
      haystack_size |-> 0,
      contains_substring |-> FALSE,
      writes |-> 0 ]

Accept(r) ==
    [ rejected |-> FALSE,
      reason |-> "none",
      index_builds |-> IF r.distinct_count = 0 THEN 0 ELSE 1,
      haystack_size |-> r.distinct_count,
      contains_substring |-> r.contains,
      writes |-> 0 ]

Evaluate(r) ==
    IF r.needle_empty THEN Reject("empty_needle")
    ELSE IF r.needle_bytes > MaxNeedleBytes THEN Reject("needle_too_long")
    ELSE IF r.raw_count > MaxHaystackTerms THEN Reject("too_many_terms")
    ELSE IF r.empty_term THEN Reject("empty_term")
    ELSE IF r.max_term_bytes > MaxTermBytes THEN Reject("term_too_long")
    ELSE IF r.total_bytes > MaxTotalBytes THEN Reject("total_too_large")
    ELSE Accept(r)

NoResp ==
    [ rejected |-> FALSE,
      reason |-> "none",
      index_builds |-> 0,
      haystack_size |-> 0,
      contains_substring |-> FALSE,
      writes |-> 0 ]

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
    /\ resp.index_builds \in 0..1
    /\ resp.haystack_size \in 0..MaxHaystackTerms
    /\ resp.contains_substring \in BOOLEAN
    /\ resp.writes = 0

InvalidInputsDoNotBuildIndex ==
    req # NoReq /\ resp.rejected => resp.index_builds = 0

AcceptedSizeIsDedupedAndBounded ==
    req # NoReq /\ ~resp.rejected =>
        /\ resp.haystack_size = req.distinct_count
        /\ resp.haystack_size <= MaxHaystackTerms

EmptyHaystackAvoidsIndex ==
    req # NoReq /\ ~resp.rejected /\ req.distinct_count = 0 =>
        /\ resp.index_builds = 0
        /\ resp.contains_substring = FALSE

ExactMembershipPreserved ==
    req # NoReq /\ ~resp.rejected => resp.contains_substring = req.contains

ReadOnly ==
    req # NoReq => resp.writes = 0

=============================================================================
