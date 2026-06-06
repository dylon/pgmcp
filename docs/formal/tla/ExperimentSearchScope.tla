------------------------------ MODULE ExperimentSearchScope ------------------------------
(***************************************************************************)
(* `experiment_search` request/filter model.                               *)
(*                                                                         *)
(* The tool validates a nonblank query, normalizes closed kind/verdict      *)
(* filters, bounds result limits, prefers vector search when embedding      *)
(* succeeds, falls back to FTS when embedding fails, and applies the same   *)
(* active-hypothesis verdict/project/kind filters in both modes.            *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

ProjectIds == {0, 1, 2}
Modes == {"vector", "fts", "none"}
Outcomes == {"ok", "rejected"}
Reasons == {"none", "blank_query", "invalid_kind", "invalid_verdict"}
Kinds == {"none", "optimization", "bugfix", "other"}
Verdicts == {"none", "pending", "accepted", "rejected", "inconclusive"}
Slugs == {"accepted", "rejected", "stale_accepted", "other_project", "bugfix"}

Experiments ==
    { [slug |-> "accepted", project |-> 1, kind |-> "optimization",
       active_verdict |-> "accepted", old_verdict |-> "none",
       text_match |-> TRUE, has_embedding |-> TRUE],
      [slug |-> "rejected", project |-> 1, kind |-> "optimization",
       active_verdict |-> "rejected", old_verdict |-> "none",
       text_match |-> TRUE, has_embedding |-> TRUE],
      [slug |-> "stale_accepted", project |-> 1, kind |-> "optimization",
       active_verdict |-> "rejected", old_verdict |-> "accepted",
       text_match |-> TRUE, has_embedding |-> TRUE],
      [slug |-> "other_project", project |-> 2, kind |-> "optimization",
       active_verdict |-> "accepted", old_verdict |-> "none",
       text_match |-> TRUE, has_embedding |-> TRUE],
      [slug |-> "bugfix", project |-> 1, kind |-> "bugfix",
       active_verdict |-> "accepted", old_verdict |-> "none",
       text_match |-> TRUE, has_embedding |-> TRUE] }

Requests ==
    { [id |-> 1, query_mode |-> "valid", embed_mode |-> "ok",
       project_filter |-> 1, kind_mode |-> "optimization",
       verdict_mode |-> "accepted", limit_mode |-> "huge"],
      [id |-> 2, query_mode |-> "valid", embed_mode |-> "fail",
       project_filter |-> 1, kind_mode |-> "optimization",
       verdict_mode |-> "accepted", limit_mode |-> "huge"],
      [id |-> 3, query_mode |-> "valid", embed_mode |-> "fail",
       project_filter |-> 0, kind_mode |-> "none",
       verdict_mode |-> "none", limit_mode |-> "negative"],
      [id |-> 4, query_mode |-> "blank", embed_mode |-> "ok",
       project_filter |-> 0, kind_mode |-> "none",
       verdict_mode |-> "none", limit_mode |-> "default"],
      [id |-> 5, query_mode |-> "valid", embed_mode |-> "ok",
       project_filter |-> 0, kind_mode |-> "bad",
       verdict_mode |-> "none", limit_mode |-> "default"],
      [id |-> 6, query_mode |-> "valid", embed_mode |-> "ok",
       project_filter |-> 0, kind_mode |-> "none",
       verdict_mode |-> "bad", limit_mode |-> "default"] }

RequestIds == {r.id : r \in Requests}

KindFor(r) ==
    IF r.kind_mode \in {"optimization", "bugfix", "other"} THEN r.kind_mode ELSE "none"

VerdictFor(r) ==
    IF r.verdict_mode \in {"pending", "accepted", "rejected", "inconclusive"} THEN r.verdict_mode ELSE "none"

LimitFor(r) ==
    CASE r.limit_mode = "huge" -> 100
      [] r.limit_mode = "negative" -> 1
      [] OTHER -> 20

ReasonFor(r) ==
    CASE r.query_mode = "blank" -> "blank_query"
      [] r.kind_mode = "bad" -> "invalid_kind"
      [] r.verdict_mode = "bad" -> "invalid_verdict"
      [] OTHER -> "none"

SearchModeFor(r) ==
    IF ReasonFor(r) # "none" THEN "none"
    ELSE IF r.embed_mode = "ok" THEN "vector" ELSE "fts"

ExperimentMatches(r, e) ==
    /\ ReasonFor(r) = "none"
    /\ (r.project_filter = 0 \/ e.project = r.project_filter)
    /\ (KindFor(r) = "none" \/ e.kind = KindFor(r))
    /\ (VerdictFor(r) = "none" \/ e.active_verdict = VerdictFor(r))
    /\ (SearchModeFor(r) = "fts" => e.text_match)
    /\ (SearchModeFor(r) = "vector" => e.has_embedding)

ResultsFor(r) ==
    IF ReasonFor(r) # "none" THEN {}
    ELSE {e.slug : e \in {x \in Experiments : ExperimentMatches(r, x)}}

ResponseFor(r) ==
    LET reason == ReasonFor(r) IN
    LET ok == reason = "none" IN
        [ request_id |-> r.id,
          outcome |-> IF ok THEN "ok" ELSE "rejected",
          reason |-> reason,
          mode |-> SearchModeFor(r),
          project_filter |-> IF ok THEN r.project_filter ELSE 0,
          kind |-> IF ok THEN KindFor(r) ELSE "none",
          verdict |-> IF ok THEN VerdictFor(r) ELSE "none",
          limit |-> IF ok THEN LimitFor(r) ELSE 0,
          results |-> ResultsFor(r),
          writes |-> 0,
          lock_held |-> FALSE ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      reason: Reasons,
      mode: Modes,
      project_filter: ProjectIds,
      kind: Kinds,
      verdict: Verdicts,
      limit: 0..100,
      results: SUBSET Slugs,
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
    ReasonFor(req) # "none" =>
        /\ response.outcome = "rejected"
        /\ response.results = {}

FiltersClosed ==
    response.outcome = "ok" =>
        /\ response.kind \in {"none", "optimization", "bugfix", "other"}
        /\ response.verdict \in {"none", "pending", "accepted", "rejected", "inconclusive"}

LimitBounded ==
    response.outcome = "ok" => response.limit \in 1..100

ModeMatchesEmbeddingOutcome ==
    response.outcome = "ok" =>
        /\ (req.embed_mode = "ok" => response.mode = "vector")
        /\ (req.embed_mode = "fail" => response.mode = "fts")

ProjectFilterSound ==
    response.outcome = "ok" /\ response.project_filter # 0 =>
        \A e \in Experiments :
            e.slug \in response.results => e.project = response.project_filter

KindFilterSound ==
    response.outcome = "ok" /\ response.kind # "none" =>
        \A e \in Experiments :
            e.slug \in response.results => e.kind = response.kind

ActiveVerdictFilterSound ==
    response.outcome = "ok" /\ response.verdict # "none" =>
        \A e \in Experiments :
            e.slug \in response.results => e.active_verdict = response.verdict

StaleVerdictsIgnored ==
    response.outcome = "ok" /\ response.verdict = "accepted" =>
        "stale_accepted" \notin response.results

FallbackFilterParity ==
    response.outcome = "ok" /\ response.mode = "fts" /\ response.project_filter = 1 /\
    response.kind = "optimization" /\ response.verdict = "accepted" =>
        response.results = {"accepted"}

ReadOnlyNoHeldLock ==
    /\ response.writes = 0
    /\ response.lock_held = FALSE

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidRequestsReject /\
        FiltersClosed /\
        LimitBounded /\
        ModeMatchesEmbeddingOutcome /\
        ProjectFilterSound /\
        KindFilterSound /\
        ActiveVerdictFilterSound /\
        StaleVerdictsIgnored /\
        FallbackFilterParity /\
        ReadOnlyNoHeldLock)

================================================================================
