----------------------------- MODULE RefactoringReportBounds -----------------------------
(***************************************************************************)
(* `refactoring_report` request-boundary model.                            *)
(*                                                                         *)
(* Similarity is modeled as a 0..100 scaled value plus a non-finite mode.   *)
(* The tool rejects non-finite similarity and oversized language filters,    *)
(* clamps min_projects/limit, computes a bounded fetch_limit, and returns    *)
(* at most the effective output limit from a read-only clustering path.      *)
(***************************************************************************)

EXTENDS Integers

MaxLimit == 5
FetchMultiplier == 5
MaxMinProjects == 8
MaxLanguageBytes == 8

SimModes == {"finite", "nonfinite"}
LangModes == {"none", "empty", "valid", "long"}
Outcomes == {"ok", "rejected"}
Reasons == {"none", "nonfinite_similarity", "language_too_large"}

Requests ==
    { [id |-> 1, sim_mode |-> "finite", raw_sim |-> 250,
       raw_min_projects |-> 0, raw_limit |-> 0, lang_mode |-> "valid",
       available_clusters |-> 3],
      [id |-> 2, sim_mode |-> "nonfinite", raw_sim |-> 85,
       raw_min_projects |-> 2, raw_limit |-> 20, lang_mode |-> "none",
       available_clusters |-> 3],
      [id |-> 3, sim_mode |-> "finite", raw_sim |-> 85,
       raw_min_projects |-> 2, raw_limit |-> 20, lang_mode |-> "long",
       available_clusters |-> 3],
      [id |-> 4, sim_mode |-> "finite", raw_sim |-> -10,
       raw_min_projects |-> 999, raw_limit |-> 999, lang_mode |-> "empty",
       available_clusters |-> 7] }

RequestIds == {r.id : r \in Requests}

Clamp(v, lo, hi) ==
    IF v < lo THEN lo ELSE IF v > hi THEN hi ELSE v

ReasonFor(r) ==
    CASE r.sim_mode = "nonfinite" -> "nonfinite_similarity"
      [] r.lang_mode = "long" -> "language_too_large"
      [] OTHER -> "none"

NormalizedLanguage(mode) ==
    CASE mode = "valid" -> "rust"
      [] OTHER -> ""

Min(a, b) == IF a <= b THEN a ELSE b

ResponseFor(r) ==
    LET reason == ReasonFor(r) IN
    LET limit == Clamp(r.raw_limit, 1, MaxLimit) IN
        [ request_id |-> r.id,
          outcome |-> IF reason = "none" THEN "ok" ELSE "rejected",
          reason |-> reason,
          min_similarity |-> IF reason = "none" THEN Clamp(r.raw_sim, 0, 100) ELSE 0,
          min_projects |-> IF reason = "none" THEN Clamp(r.raw_min_projects, 1, MaxMinProjects) ELSE 0,
          language |-> IF reason = "none" THEN NormalizedLanguage(r.lang_mode) ELSE "",
          limit |-> IF reason = "none" THEN limit ELSE 0,
          fetch_limit |-> IF reason = "none" THEN limit * FetchMultiplier ELSE 0,
          candidate_count |-> IF reason = "none" THEN Min(r.available_clusters, limit) ELSE 0,
          writes |-> 0,
          lock_held |-> FALSE ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      reason: Reasons,
      min_similarity: 0..100,
      min_projects: 0..MaxMinProjects,
      language: {"", "rust"},
      limit: 0..MaxLimit,
      fetch_limit: 0..(MaxLimit * FetchMultiplier),
      candidate_count: 0..MaxLimit,
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

SimilarityIsFiniteAndBounded ==
    response.outcome = "ok" =>
        /\ req.sim_mode = "finite"
        /\ response.min_similarity \in 0..100

LanguageNormalizedAndBounded ==
    response.outcome = "ok" =>
        /\ req.lang_mode # "long"
        /\ response.language \in {"", "rust"}

LimitsAndFetchAreBounded ==
    response.outcome = "ok" =>
        /\ response.limit \in 1..MaxLimit
        /\ response.fetch_limit = response.limit * FetchMultiplier
        /\ response.fetch_limit <= MaxLimit * FetchMultiplier

MinProjectsBounded ==
    response.outcome = "ok" => response.min_projects \in 1..MaxMinProjects

CandidatesDoNotExceedLimit ==
    response.outcome = "ok" => response.candidate_count <= response.limit

ReadOnlyNoHeldLock ==
    /\ response.writes = 0
    /\ response.lock_held = FALSE

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidRequestsReject /\
        SimilarityIsFiniteAndBounded /\
        LanguageNormalizedAndBounded /\
        LimitsAndFetchAreBounded /\
        MinProjectsBounded /\
        CandidatesDoNotExceedLimit /\
        ReadOnlyNoHeldLock)

================================================================================
