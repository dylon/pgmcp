------------------------------ MODULE HybridSearchBoundary ------------------------------
(***************************************************************************)
(* Direct `hybrid_search` numeric/degradation boundary.                     *)
(*                                                                         *)
(* This complements SearchToolScoping.tla, which covers project/language    *)
(* result scoping. Here we model the local request normalization and leg     *)
(* composition properties that keep a bad request from expanding into a      *)
(* huge fetch/truncate window or poisoning RRF with non-finite weights.      *)
(***************************************************************************)

EXTENDS Naturals, Integers

MaxLimit == 100
MaxDistance == 64

LegOutcomes == {"ok", "skipped", "error", "timeout"}
WeightKinds == {"finite", "nan", "inf"}
Reasons == {"none", "bad_weight"}

NoReq ==
    [ id |-> 0,
      project |-> "none",
      language |-> "none",
      limit |-> 20,
      edit_distance |-> 2,
      bm25_kind |-> "finite",
      semantic_kind |-> "finite",
      wfst_kind |-> "finite",
      bm25_weight |-> 1,
      semantic_weight |-> 1,
      wfst_weight |-> 1,
      text_outcome |-> "ok",
      semantic_outcome |-> "ok",
      model_exists |-> TRUE,
      available_text |-> 0,
      available_semantic |-> 0,
      available_wfst |-> 0 ]

Requests ==
    { [ id |-> 1, project |-> "none", language |-> "none", limit |-> -1,
        edit_distance |-> 999, bm25_kind |-> "finite", semantic_kind |-> "finite",
        wfst_kind |-> "finite", bm25_weight |-> 1, semantic_weight |-> 1,
        wfst_weight |-> 1, text_outcome |-> "ok", semantic_outcome |-> "ok",
        model_exists |-> FALSE, available_text |-> 3, available_semantic |-> 3,
        available_wfst |-> 0 ],
      [ id |-> 2, project |-> "p", language |-> " rust ", limit |-> 999,
        edit_distance |-> 999, bm25_kind |-> "finite", semantic_kind |-> "finite",
        wfst_kind |-> "finite", bm25_weight |-> 1, semantic_weight |-> 1,
        wfst_weight |-> 1, text_outcome |-> "timeout", semantic_outcome |-> "ok",
        model_exists |-> TRUE, available_text |-> 3, available_semantic |-> 7,
        available_wfst |-> 5 ],
      [ id |-> 3, project |-> "p", language |-> "none", limit |-> 20,
        edit_distance |-> 2, bm25_kind |-> "nan", semantic_kind |-> "finite",
        wfst_kind |-> "finite", bm25_weight |-> 1, semantic_weight |-> 1,
        wfst_weight |-> 1, text_outcome |-> "ok", semantic_outcome |-> "ok",
        model_exists |-> TRUE, available_text |-> 3, available_semantic |-> 3,
        available_wfst |-> 3 ],
      [ id |-> 4, project |-> " p ", language |-> "none", limit |-> 10,
        edit_distance |-> 2, bm25_kind |-> "finite", semantic_kind |-> "finite",
        wfst_kind |-> "finite", bm25_weight |-> 0, semantic_weight |-> 1,
        wfst_weight |-> 0, text_outcome |-> "ok", semantic_outcome |-> "error",
        model_exists |-> TRUE, available_text |-> 3, available_semantic |-> 3,
        available_wfst |-> 3 ],
      [ id |-> 5, project |-> "p", language |-> "none", limit |-> 10,
        edit_distance |-> 2, bm25_kind |-> "finite", semantic_kind |-> "inf",
        wfst_kind |-> "finite", bm25_weight |-> 1, semantic_weight |-> 1,
        wfst_weight |-> 1, text_outcome |-> "ok", semantic_outcome |-> "ok",
        model_exists |-> TRUE, available_text |-> 3, available_semantic |-> 3,
        available_wfst |-> 3 ],
      [ id |-> 6, project |-> "   ", language |-> "   ", limit |-> 0,
        edit_distance |-> 0, bm25_kind |-> "finite", semantic_kind |-> "finite",
        wfst_kind |-> "finite", bm25_weight |-> 1, semantic_weight |-> 0,
        wfst_weight |-> 1, text_outcome |-> "ok", semantic_outcome |-> "ok",
        model_exists |-> TRUE, available_text |-> 8, available_semantic |-> 8,
        available_wfst |-> 8 ] }

TrimOptional(v) ==
    CASE v = " p " -> "p"
      [] v = " rust " -> "rust"
      [] v = "   " -> "none"
      [] OTHER -> v

ClampLimit(n) ==
    IF n < 1 THEN 1 ELSE IF n > MaxLimit THEN MaxLimit ELSE n

ClampDistance(n) ==
    IF n > MaxDistance THEN MaxDistance ELSE n

Min(a, b) == IF a < b THEN a ELSE b

BadWeight(r) ==
    \/ r.bm25_kind # "finite"
    \/ r.semantic_kind # "finite"
    \/ r.wfst_kind # "finite"

RunText(r) == r.bm25_weight > 0
RunSemantic(r) == r.semantic_weight > 0
RunWfst(r) ==
    /\ r.wfst_weight > 0
    /\ TrimOptional(r.project) # "none"
    /\ r.model_exists

LegStatus(run, outcome) ==
    IF ~run THEN "skipped" ELSE outcome

Contrib(status, available, limit) ==
    IF status = "ok" THEN Min(available, limit) ELSE 0

Reject(reason) ==
    [ rejected |-> TRUE,
      reason |-> reason,
      normalized_project |-> "none",
      normalized_language |-> "none",
      effective_limit |-> 20,
      fetch_window |-> 0,
      effective_edit_distance |-> 2,
      text_status |-> "skipped",
      semantic_status |-> "skipped",
      wfst_status |-> "skipped",
      degraded |-> FALSE,
      text_rows |-> 0,
      semantic_rows |-> 0,
      wfst_rows |-> 0,
      fused_count |-> 0,
      db_writes |-> 0,
      retained_locks |-> 0 ]

Accept(r) ==
    LET lim == ClampLimit(r.limit) IN
    LET text_status == LegStatus(RunText(r), r.text_outcome) IN
    LET semantic_status == LegStatus(RunSemantic(r), r.semantic_outcome) IN
    LET wfst_status == IF RunWfst(r) THEN "ok" ELSE "skipped" IN
    LET text_rows == Contrib(text_status, r.available_text, lim * 2) IN
    LET semantic_rows == Contrib(semantic_status, r.available_semantic, lim * 2) IN
    LET wfst_rows == Contrib(wfst_status, r.available_wfst, lim * 2) IN
    [ rejected |-> FALSE,
      reason |-> "none",
      normalized_project |-> TrimOptional(r.project),
      normalized_language |-> TrimOptional(r.language),
      effective_limit |-> lim,
      fetch_window |-> lim * 2,
      effective_edit_distance |-> ClampDistance(r.edit_distance),
      text_status |-> text_status,
      semantic_status |-> semantic_status,
      wfst_status |-> wfst_status,
      degraded |-> text_status \in {"error", "timeout"} \/ semantic_status \in {"error", "timeout"},
      text_rows |-> text_rows,
      semantic_rows |-> semantic_rows,
      wfst_rows |-> wfst_rows,
      fused_count |-> Min(text_rows + semantic_rows + wfst_rows, lim),
      db_writes |-> 0,
      retained_locks |-> 0 ]

Evaluate(r) ==
    IF BadWeight(r) THEN Reject("bad_weight") ELSE Accept(r)

NoResp ==
    [ rejected |-> FALSE,
      reason |-> "none",
      normalized_project |-> "none",
      normalized_language |-> "none",
      effective_limit |-> 20,
      fetch_window |-> 0,
      effective_edit_distance |-> 2,
      text_status |-> "skipped",
      semantic_status |-> "skipped",
      wfst_status |-> "skipped",
      degraded |-> FALSE,
      text_rows |-> 0,
      semantic_rows |-> 0,
      wfst_rows |-> 0,
      fused_count |-> 0,
      db_writes |-> 0,
      retained_locks |-> 0 ]

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
    /\ resp.normalized_project \in {"none", "p"}
    /\ resp.normalized_language \in {"none", "rust"}
    /\ resp.effective_limit \in 1..MaxLimit
    /\ resp.fetch_window \in 0..(MaxLimit * 2)
    /\ resp.effective_edit_distance \in 0..MaxDistance
    /\ resp.text_status \in LegOutcomes
    /\ resp.semantic_status \in LegOutcomes
    /\ resp.wfst_status \in LegOutcomes
    /\ resp.degraded \in BOOLEAN
    /\ resp.text_rows \in 0..(MaxLimit * 2)
    /\ resp.semantic_rows \in 0..(MaxLimit * 2)
    /\ resp.wfst_rows \in 0..(MaxLimit * 2)
    /\ resp.fused_count \in 0..MaxLimit
    /\ resp.db_writes = 0
    /\ resp.retained_locks = 0

BadWeightsRejectBeforeLegs ==
    req # NoReq /\ BadWeight(req) =>
        /\ resp.rejected
        /\ resp.reason = "bad_weight"
        /\ resp.text_status = "skipped"
        /\ resp.semantic_status = "skipped"
        /\ resp.wfst_status = "skipped"
        /\ resp.fused_count = 0

BoundsBeforeFetchAndTruncate ==
    req # NoReq /\ ~resp.rejected =>
        /\ resp.effective_limit <= MaxLimit
        /\ resp.fetch_window = resp.effective_limit * 2
        /\ resp.fetch_window <= MaxLimit * 2
        /\ resp.fused_count <= resp.effective_limit
        /\ resp.effective_edit_distance <= MaxDistance

ZeroWeightsSkipLegs ==
    req # NoReq /\ ~resp.rejected =>
        /\ req.bm25_weight = 0 => resp.text_status = "skipped"
        /\ req.semantic_weight = 0 => resp.semantic_status = "skipped"
        /\ req.wfst_weight = 0 => resp.wfst_status = "skipped"

LegFailuresDegradeOnly ==
    req # NoReq /\ ~resp.rejected =>
        resp.degraded = (resp.text_status \in {"error", "timeout"} \/
                         resp.semantic_status \in {"error", "timeout"})

ThirdLegRequiresProjectAndModel ==
    req # NoReq /\ ~resp.rejected /\ resp.wfst_status = "ok" =>
        /\ resp.normalized_project # "none"
        /\ req.model_exists
        /\ req.wfst_weight > 0

NoDbMutationOrRetainedLocks ==
    req # NoReq =>
        /\ resp.db_writes = 0
        /\ resp.retained_locks = 0

=============================================================================
