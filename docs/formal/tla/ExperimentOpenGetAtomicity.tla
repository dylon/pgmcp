-------------------------- MODULE ExperimentOpenGetAtomicity --------------------------
(***************************************************************************)
(* `experiment_open` / `experiment_get` request boundary.                  *)
(*                                                                         *)
(* `experiment_open` pre-registers an experiment and its first hypothesis. *)
(* The core row and hypothesis must commit atomically. `experiment_get`     *)
(* must reject missing identifiers, normalize slug lookups, and never       *)
(* return a partially-opened experiment without a hypothesis.               *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

NoId == -999

Kinds == {"optimization", "feature_refactor", "feature_addition", "bugfix", "investigation", "other"}
Directions == {"increase", "decrease", "either", "none"}
Projects == {7}
Modes == {"open", "get"}
Outcomes == {"ok", "rejected"}
Reasons ==
    {"none", "blank_required", "unknown_kind", "unknown_direction",
     "unknown_project", "hypothesis_failure", "missing_lookup",
     "bad_id", "not_found"}
Sources == {"none", "id", "slug"}
Slugs == {"none", "arena-dispatch-normalized", "missing-slug"}

Requests ==
    { [ id |-> 1, mode |-> "open", raw_title |-> "  Arena allocation  ",
        raw_question |-> "  Does it help?  ", raw_hypothesis |-> "  It helps  ",
        raw_metric |-> " latency_ms ", raw_kind |-> " optimization ",
        raw_direction |-> " either ", raw_slug |-> " arena-dispatch-normalized ",
        project_id |-> NoId, hypothesis_ok |-> TRUE, lookup_id |-> NoId ],
      [ id |-> 2, mode |-> "open", raw_title |-> "   ",
        raw_question |-> "q", raw_hypothesis |-> "h", raw_metric |-> "m",
        raw_kind |-> "optimization", raw_direction |-> "either", raw_slug |-> "",
        project_id |-> NoId, hypothesis_ok |-> TRUE, lookup_id |-> NoId ],
      [ id |-> 3, mode |-> "open", raw_title |-> "t",
        raw_question |-> "q", raw_hypothesis |-> "h", raw_metric |-> "m",
        raw_kind |-> "slow", raw_direction |-> "either", raw_slug |-> "",
        project_id |-> NoId, hypothesis_ok |-> TRUE, lookup_id |-> NoId ],
      [ id |-> 4, mode |-> "open", raw_title |-> "t",
        raw_question |-> "q", raw_hypothesis |-> "h", raw_metric |-> "m",
        raw_kind |-> "optimization", raw_direction |-> "sideways", raw_slug |-> "",
        project_id |-> NoId, hypothesis_ok |-> TRUE, lookup_id |-> NoId ],
      [ id |-> 5, mode |-> "open", raw_title |-> "t",
        raw_question |-> "q", raw_hypothesis |-> "h", raw_metric |-> "m",
        raw_kind |-> "optimization", raw_direction |-> "either", raw_slug |-> "",
        project_id |-> 999, hypothesis_ok |-> TRUE, lookup_id |-> NoId ],
      [ id |-> 6, mode |-> "open", raw_title |-> "t",
        raw_question |-> "q", raw_hypothesis |-> "h", raw_metric |-> "m",
        raw_kind |-> "optimization", raw_direction |-> "either", raw_slug |-> "",
        project_id |-> NoId, hypothesis_ok |-> FALSE, lookup_id |-> NoId ],
      [ id |-> 7, mode |-> "get", raw_title |-> "", raw_question |-> "",
        raw_hypothesis |-> "", raw_metric |-> "", raw_kind |-> "",
        raw_direction |-> "", raw_slug |-> "", project_id |-> NoId,
        hypothesis_ok |-> TRUE, lookup_id |-> NoId ],
      [ id |-> 8, mode |-> "get", raw_title |-> "", raw_question |-> "",
        raw_hypothesis |-> "", raw_metric |-> "", raw_kind |-> "",
        raw_direction |-> "", raw_slug |-> "   ", project_id |-> NoId,
        hypothesis_ok |-> TRUE, lookup_id |-> NoId ],
      [ id |-> 9, mode |-> "get", raw_title |-> "", raw_question |-> "",
        raw_hypothesis |-> "", raw_metric |-> "", raw_kind |-> "",
        raw_direction |-> "", raw_slug |-> "", project_id |-> NoId,
        hypothesis_ok |-> TRUE, lookup_id |-> -1 ],
      [ id |-> 10, mode |-> "get", raw_title |-> "", raw_question |-> "",
        raw_hypothesis |-> "", raw_metric |-> "", raw_kind |-> "",
        raw_direction |-> "", raw_slug |-> " arena-dispatch-normalized ",
        project_id |-> NoId, hypothesis_ok |-> TRUE, lookup_id |-> NoId ],
      [ id |-> 11, mode |-> "get", raw_title |-> "", raw_question |-> "",
        raw_hypothesis |-> "", raw_metric |-> "", raw_kind |-> "",
        raw_direction |-> "", raw_slug |-> "missing-slug", project_id |-> NoId,
        hypothesis_ok |-> TRUE, lookup_id |-> NoId ],
      [ id |-> 12, mode |-> "get", raw_title |-> "", raw_question |-> "",
        raw_hypothesis |-> "", raw_metric |-> "", raw_kind |-> "",
        raw_direction |-> "", raw_slug |-> "missing-slug", project_id |-> NoId,
        hypothesis_ok |-> TRUE, lookup_id |-> 1 ] }

RequestIds == {r.id : r \in Requests}

Normalize(raw) ==
    CASE raw = "  Arena allocation  " -> "Arena allocation"
      [] raw = "  Does it help?  " -> "Does it help?"
      [] raw = "  It helps  " -> "It helps"
      [] raw = " latency_ms " -> "latency_ms"
      [] raw = " optimization " -> "optimization"
      [] raw = " either " -> "either"
      [] raw = " arena-dispatch-normalized " -> "arena-dispatch-normalized"
      [] raw = "   " -> ""
      [] OTHER -> raw

SlugForOpen(r) ==
    LET s == Normalize(r.raw_slug) IN IF s = "" THEN "arena-dispatch-normalized" ELSE s

OpenReason(r) ==
    LET title == Normalize(r.raw_title) IN
    LET question == Normalize(r.raw_question) IN
    LET hypothesis == Normalize(r.raw_hypothesis) IN
    LET metric == Normalize(r.raw_metric) IN
    LET kind == Normalize(r.raw_kind) IN
    LET direction == Normalize(r.raw_direction) IN
        CASE title = "" \/ question = "" \/ hypothesis = "" \/ metric = "" -> "blank_required"
          [] ~(kind \in Kinds) -> "unknown_kind"
          [] direction = "" -> "none"
          [] ~(direction \in Directions) -> "unknown_direction"
          [] r.project_id # NoId /\ ~(r.project_id \in Projects) -> "unknown_project"
          [] ~r.hypothesis_ok -> "hypothesis_failure"
          [] OTHER -> "none"

GetSlug(r) == Normalize(r.raw_slug)

GetSource(r) ==
    IF r.lookup_id # NoId THEN "id"
    ELSE IF GetSlug(r) # "" THEN "slug"
    ELSE "none"

ExistingById(id) == id = 1
ExistingBySlug(slug) == slug = "arena-dispatch-normalized"

GetReason(r) ==
    CASE r.lookup_id # NoId /\ r.lookup_id <= 0 -> "bad_id"
      [] GetSource(r) = "none" -> "missing_lookup"
      [] GetSource(r) = "id" /\ ~ExistingById(r.lookup_id) -> "not_found"
      [] GetSource(r) = "slug" /\ ~ExistingBySlug(GetSlug(r)) -> "not_found"
      [] OTHER -> "none"

ReasonFor(r) == IF r.mode = "open" THEN OpenReason(r) ELSE GetReason(r)

ResponseFor(r) ==
    LET reason == ReasonFor(r) IN
    LET source == IF r.mode = "get" THEN GetSource(r) ELSE "none" IN
        IF reason # "none" THEN
            [ request_id |-> r.id,
              mode |-> r.mode,
              outcome |-> "rejected",
              reason |-> reason,
              experiment_exists |-> FALSE,
              hypothesis_exists |-> FALSE,
              slug |-> "none",
              kind |-> "other",
              direction |-> "either",
              lookup_source |-> source ]
        ELSE IF r.mode = "open" THEN
            [ request_id |-> r.id,
              mode |-> "open",
              outcome |-> "ok",
              reason |-> "none",
              experiment_exists |-> TRUE,
              hypothesis_exists |-> TRUE,
              slug |-> SlugForOpen(r),
              kind |-> Normalize(r.raw_kind),
              direction |-> IF Normalize(r.raw_direction) = "" THEN "either" ELSE Normalize(r.raw_direction),
              lookup_source |-> "none" ]
        ELSE
            [ request_id |-> r.id,
              mode |-> "get",
              outcome |-> "ok",
              reason |-> "none",
              experiment_exists |-> TRUE,
              hypothesis_exists |-> TRUE,
              slug |-> "arena-dispatch-normalized",
              kind |-> "optimization",
              direction |-> "either",
              lookup_source |-> source ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      mode: Modes,
      outcome: Outcomes,
      reason: Reasons,
      experiment_exists: BOOLEAN,
      hypothesis_exists: BOOLEAN,
      slug: Slugs,
      kind: Kinds,
      direction: Directions,
      lookup_source: Sources ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests
    /\ response \in ResponseRecord

OpenCommitsExperimentAndHypothesisTogether ==
    req.mode = "open" =>
        IF response.outcome = "ok"
        THEN /\ response.experiment_exists = TRUE
             /\ response.hypothesis_exists = TRUE
        ELSE /\ response.experiment_exists = FALSE
             /\ response.hypothesis_exists = FALSE

HypothesisFailureRollsBackExperiment ==
    req.mode = "open" /\ ~req.hypothesis_ok =>
        /\ response.reason = "hypothesis_failure"
        /\ response.experiment_exists = FALSE
        /\ response.hypothesis_exists = FALSE

OpenFieldsNormalized ==
    req.mode = "open" /\ response.outcome = "ok" =>
        /\ response.slug = SlugForOpen(req)
        /\ response.kind = Normalize(req.raw_kind)
        /\ response.direction = IF Normalize(req.raw_direction) = "" THEN "either" ELSE Normalize(req.raw_direction)

InvalidOpenRejectedBeforeWrite ==
    req.mode = "open" /\ OpenReason(req) # "none" =>
        /\ response.outcome = "rejected"
        /\ response.experiment_exists = FALSE
        /\ response.hypothesis_exists = FALSE

GetRequiresLookup ==
    req.mode = "get" /\ GetSource(req) = "none" =>
        response.reason = "missing_lookup"

GetRejectsBadId ==
    req.mode = "get" /\ req.lookup_id # NoId /\ req.lookup_id <= 0 =>
        response.reason = "bad_id"

GetSlugTrimmed ==
    req.mode = "get" /\ GetSlug(req) = "arena-dispatch-normalized" =>
        response.outcome = "ok"

GetIdWinsOverSlug ==
    req.mode = "get" /\ req.lookup_id = 1 /\ GetSlug(req) = "missing-slug" =>
        /\ response.outcome = "ok"
        /\ response.lookup_source = "id"

GetOkHasHypothesis ==
    req.mode = "get" /\ response.outcome = "ok" =>
        /\ response.experiment_exists = TRUE
        /\ response.hypothesis_exists = TRUE

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        OpenCommitsExperimentAndHypothesisTogether /\
        HypothesisFailureRollsBackExperiment /\
        OpenFieldsNormalized /\
        InvalidOpenRejectedBeforeWrite /\
        GetRequiresLookup /\
        GetRejectsBadId /\
        GetSlugTrimmed /\
        GetIdWinsOverSlug /\
        GetOkHasHypothesis)

=============================================================================
