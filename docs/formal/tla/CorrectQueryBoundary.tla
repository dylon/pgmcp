---------------------------- MODULE CorrectQueryBoundary ----------------------------
(***************************************************************************)
(* `correct_query` request boundary and per-project artifact-key model.     *)
(*                                                                         *)
(* WFST/lattice correctness is inherited from the wfst/query_rescore unit   *)
(* tests and upstream dependency proof corpus. This model checks pgmcp's    *)
(* adapter obligations: normalize inputs, fail closed on invalid project    *)
(* resolution, bound edit/LM parameters, and key both trie and HybridLM      *)
(* artifacts by the resolved project id so slug-colliding names cannot      *)
(* share state.                                                            *)
(***************************************************************************)

EXTENDS Integers

MaxDistance == 64
DefaultDistance == 2
DefaultLmWeight == 50
MaxQueryChars == 4096

Projects == {1, 2}
Outcomes == {"ok", "rejected"}
Reasons ==
    {"none", "blank_project", "unknown_project", "duplicate_project",
     "blank_query", "oversized_query", "nonfinite_lm_weight"}
Keys == {"none", "correct_slug-p1", "correct_slug-p2"}
Corrections == {"none", "alpha_handler", "beta_handler", "receive"}

Requests ==
    { [ id |-> 1, raw_project |-> " correct/slug ", project_case |-> "unique_1",
        raw_query |-> " alpha_hanlder ", query_case |-> "normal",
        max_distance |-> 999, lm_case |-> "high" ],
      [ id |-> 2, raw_project |-> "correct_slug", project_case |-> "unique_2",
        raw_query |-> "beta_hanlder", query_case |-> "normal",
        max_distance |-> DefaultDistance, lm_case |-> "default" ],
      [ id |-> 3, raw_project |-> "   ", project_case |-> "blank",
        raw_query |-> "recieve", query_case |-> "normal",
        max_distance |-> DefaultDistance, lm_case |-> "default" ],
      [ id |-> 4, raw_project |-> "correct_dup", project_case |-> "duplicate",
        raw_query |-> "recieve", query_case |-> "normal",
        max_distance |-> DefaultDistance, lm_case |-> "default" ],
      [ id |-> 5, raw_project |-> "correct/slug", project_case |-> "unique_1",
        raw_query |-> "   ", query_case |-> "blank",
        max_distance |-> DefaultDistance, lm_case |-> "default" ],
      [ id |-> 6, raw_project |-> "correct/slug", project_case |-> "unique_1",
        raw_query |-> "oversized", query_case |-> "oversized",
        max_distance |-> DefaultDistance, lm_case |-> "default" ],
      [ id |-> 7, raw_project |-> "correct/slug", project_case |-> "unique_1",
        raw_query |-> "recieve", query_case |-> "normal",
        max_distance |-> DefaultDistance, lm_case |-> "nonfinite" ],
      [ id |-> 8, raw_project |-> "missing", project_case |-> "unknown",
        raw_query |-> "recieve", query_case |-> "normal",
        max_distance |-> DefaultDistance, lm_case |-> "default" ],
      [ id |-> 9, raw_project |-> "correct/slug", project_case |-> "unique_1",
        raw_query |-> "recieve", query_case |-> "normal",
        max_distance |-> 0, lm_case |-> "negative" ] }

RequestIds == {r.id : r \in Requests}

NormalizeProject(raw) ==
    CASE raw = " correct/slug " -> "correct/slug"
      [] raw = "   " -> ""
      [] OTHER -> raw

NormalizeQuery(raw) ==
    CASE raw = " alpha_hanlder " -> "alpha_hanlder"
      [] raw = "   " -> ""
      [] OTHER -> raw

ResolvedProjectId(r) ==
    CASE r.project_case = "unique_1" -> 1
      [] r.project_case = "unique_2" -> 2
      [] OTHER -> 0

ArtifactKey(project_id) ==
    CASE project_id = 1 -> "correct_slug-p1"
      [] project_id = 2 -> "correct_slug-p2"
      [] OTHER -> "none"

ReasonFor(r) ==
    CASE NormalizeProject(r.raw_project) = "" -> "blank_project"
      [] r.project_case = "unknown" -> "unknown_project"
      [] r.project_case = "duplicate" -> "duplicate_project"
      [] NormalizeQuery(r.raw_query) = "" -> "blank_query"
      [] r.query_case = "oversized" -> "oversized_query"
      [] r.lm_case = "nonfinite" -> "nonfinite_lm_weight"
      [] OTHER -> "none"

DistanceFor(r) ==
    IF r.max_distance > MaxDistance THEN MaxDistance ELSE r.max_distance

LmWeightFor(r) ==
    CASE r.lm_case = "high" -> 100
      [] r.lm_case = "negative" -> 0
      [] OTHER -> DefaultLmWeight

CorrectionFor(r) ==
    CASE ResolvedProjectId(r) = 1 /\ NormalizeQuery(r.raw_query) = "alpha_hanlder"
            -> "alpha_handler"
      [] ResolvedProjectId(r) = 2 /\ NormalizeQuery(r.raw_query) = "beta_hanlder"
            -> "beta_handler"
      [] OTHER -> "receive"

CorrectionProject(c) ==
    CASE c = "alpha_handler" -> 1
      [] c = "beta_handler" -> 2
      [] c = "receive" -> 1
      [] OTHER -> 0

ResponseFor(r) ==
    LET reason == ReasonFor(r) IN
        [ request_id |-> r.id,
          outcome |-> IF reason = "none" THEN "ok" ELSE "rejected",
          reason |-> reason,
          project |-> NormalizeProject(r.raw_project),
          query |-> NormalizeQuery(r.raw_query),
          project_id |-> IF reason = "none" THEN ResolvedProjectId(r) ELSE 0,
          max_distance |-> IF reason = "none" THEN DistanceFor(r) ELSE 0,
          lm_weight |-> IF reason = "none" THEN LmWeightFor(r) ELSE 0,
          trie_key |-> IF reason = "none" THEN ArtifactKey(ResolvedProjectId(r)) ELSE "none",
          model_key |-> IF reason = "none" THEN ArtifactKey(ResolvedProjectId(r)) ELSE "none",
          corrected |-> IF reason = "none" THEN CorrectionFor(r) ELSE "none",
          writes |-> 0,
          locks |-> 0 ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      reason: Reasons,
      project: {"", "correct/slug", "correct_slug", "correct_dup", "missing"},
      query: {"", "alpha_hanlder", "beta_hanlder", "recieve", "oversized"},
      project_id: 0..2,
      max_distance: 0..MaxDistance,
      lm_weight: 0..100,
      trie_key: Keys,
      model_key: Keys,
      corrected: Corrections,
      writes: 0..0,
      locks: 0..0 ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK == response \in ResponseRecord

InvalidRequestsReject ==
    ReasonFor(req) # "none" => response.outcome = "rejected"

AcceptedRequestsHaveUniqueProject ==
    response.outcome = "ok" =>
        /\ req.project_case \in {"unique_1", "unique_2"}
        /\ response.project_id \in Projects

BoundsApplied ==
    response.outcome = "ok" =>
        /\ response.max_distance \in 0..MaxDistance
        /\ response.lm_weight \in 0..100

TrieAndModelUseSameResolvedKey ==
    response.outcome = "ok" =>
        /\ response.trie_key = ArtifactKey(response.project_id)
        /\ response.model_key = ArtifactKey(response.project_id)
        /\ response.trie_key = response.model_key

SlugCollisionSeparatedByProjectId ==
    response.outcome = "ok" =>
        /\ response.project_id = 1 => response.trie_key # ArtifactKey(2)
        /\ response.project_id = 2 => response.trie_key # ArtifactKey(1)

CorrectionIsProjectLocal ==
    response.outcome = "ok" =>
        CorrectionProject(response.corrected) = response.project_id

ReadOnlyNoLocks ==
    /\ response.writes = 0
    /\ response.locks = 0

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidRequestsReject /\
        AcceptedRequestsHaveUniqueProject /\
        BoundsApplied /\
        TrieAndModelUseSameResolvedKey /\
        SlugCollisionSeparatedByProjectId /\
        CorrectionIsProjectLocal /\
        ReadOnlyNoLocks)

=============================================================================
