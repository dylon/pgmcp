--------------------------- MODULE FindCoupledFilesScope ---------------------------
(***************************************************************************)
(* `find_coupled_files` request-boundary and project-scoping model.        *)
(*                                                                         *)
(* The tool trims the project name, rejects blank/unknown/duplicate project *)
(* identities, clamps bounded numeric parameters, queries by resolved        *)
(* project_id, excludes bulk commits, truncates output, and performs no      *)
(* writes or process-level locking.                                         *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

MaxLimit == 200
DefaultLimit == 50
DefaultMinCoupling == 30
DefaultMinCommits == 3
MaxMinCommits == 10000

Projects == {1, 2}
Pairs == {"AB", "CD", "AC", "AD", "BC", "BD", "AE", "Bulk", "Shadow"}
Outcomes == {"ok", "rejected", "no_data"}
Reasons == {"none", "blank_project", "unknown_project", "duplicate_project",
             "bad_min_coupling"}

Requests ==
    { [ id |-> 1, raw_project |-> " git-coupled ", project_case |-> "unique",
        has_git |-> TRUE, min_coupling_mode |-> "valid40",
        min_commits |-> 1, limit |-> 50 ],
      [ id |-> 2, raw_project |-> "   ", project_case |-> "unique",
        has_git |-> TRUE, min_coupling_mode |-> "default",
        min_commits |-> 3, limit |-> 50 ],
      [ id |-> 3, raw_project |-> "git-coupled", project_case |-> "duplicate",
        has_git |-> TRUE, min_coupling_mode |-> "valid40",
        min_commits |-> 1, limit |-> 50 ],
      [ id |-> 4, raw_project |-> "missing", project_case |-> "unknown",
        has_git |-> TRUE, min_coupling_mode |-> "default",
        min_commits |-> 3, limit |-> 50 ],
      [ id |-> 5, raw_project |-> "git-coupled", project_case |-> "unique",
        has_git |-> TRUE, min_coupling_mode |-> "negative",
        min_commits |-> -9, limit |-> -3 ],
      [ id |-> 6, raw_project |-> "git-coupled", project_case |-> "unique",
        has_git |-> TRUE, min_coupling_mode |-> "above100",
        min_commits |-> 1, limit |-> 50 ],
      [ id |-> 7, raw_project |-> "git-coupled", project_case |-> "unique",
        has_git |-> FALSE, min_coupling_mode |-> "default",
        min_commits |-> 3, limit |-> 50 ],
      [ id |-> 8, raw_project |-> "git-coupled", project_case |-> "unique",
        has_git |-> TRUE, min_coupling_mode |-> "nonfinite",
        min_commits |-> 3, limit |-> 50 ] }

RequestIds == {r.id : r \in Requests}

NormalizeProject(raw) ==
    CASE raw = " git-coupled " -> "git-coupled"
      [] raw = "   " -> ""
      [] OTHER -> raw

ResolvedProjectId(r) ==
    IF r.project_case = "unique" THEN 1 ELSE 0

ReasonFor(r) ==
    CASE NormalizeProject(r.raw_project) = "" -> "blank_project"
      [] r.project_case = "unknown" -> "unknown_project"
      [] r.project_case = "duplicate" -> "duplicate_project"
      [] r.min_coupling_mode = "nonfinite" -> "bad_min_coupling"
      [] OTHER -> "none"

MinCouplingFor(r) ==
    CASE r.min_coupling_mode = "default" -> DefaultMinCoupling
      [] r.min_coupling_mode = "negative" -> 0
      [] r.min_coupling_mode = "valid40" -> 40
      [] r.min_coupling_mode = "above100" -> 100
      [] OTHER -> DefaultMinCoupling

MinCommitsFor(r) ==
    IF r.min_commits < 1 THEN 1
    ELSE IF r.min_commits > MaxMinCommits THEN MaxMinCommits
    ELSE r.min_commits

LimitFor(r) ==
    IF r.limit < 1 THEN 1
    ELSE IF r.limit > MaxLimit THEN MaxLimit
    ELSE r.limit

PairProject(p) ==
    CASE p = "Shadow" -> 2
      [] OTHER -> 1

PairBulk(p) == p = "Bulk"

PairJaccard(p) ==
    CASE p = "AB" -> 100
      [] p = "CD" -> 50
      [] p \in {"AD", "BD"} -> 33
      [] p \in {"AC", "BC"} -> 25
      [] p = "AE" -> 0
      [] p = "Bulk" -> 100
      [] p = "Shadow" -> 100

PairCoCommits(p) ==
    CASE p = "AB" -> 3
      [] p = "CD" -> 1
      [] p \in {"AC", "AD", "BC", "BD"} -> 1
      [] p = "AE" -> 0
      [] p = "Bulk" -> 100
      [] p = "Shadow" -> 4

EligiblePairs(r) ==
    {p \in Pairs :
        /\ PairProject(p) = ResolvedProjectId(r)
        /\ ~PairBulk(p)
        /\ PairJaccard(p) >= MinCouplingFor(r)
        /\ PairCoCommits(p) >= MinCommitsFor(r)}

Min(a, b) == IF a <= b THEN a ELSE b

ResponseFor(r) ==
    LET reason == ReasonFor(r) IN
    LET eligible == IF reason = "none" /\ r.has_git THEN EligiblePairs(r) ELSE {} IN
        [ request_id |-> r.id,
          outcome |-> IF reason # "none" THEN "rejected"
                      ELSE IF ~r.has_git THEN "no_data"
                      ELSE "ok",
          reason |-> reason,
          project |-> NormalizeProject(r.raw_project),
          project_id |-> IF reason = "none" THEN ResolvedProjectId(r) ELSE 0,
          min_coupling |-> IF reason = "none" THEN MinCouplingFor(r) ELSE 0,
          min_commits |-> IF reason = "none" THEN MinCommitsFor(r) ELSE 0,
          limit |-> IF reason = "none" THEN LimitFor(r) ELSE 0,
          candidate_pairs |-> eligible,
          result_count |-> IF reason = "none" /\ r.has_git
                           THEN Min(Cardinality(eligible), LimitFor(r))
                           ELSE 0,
          writes |-> 0,
          locks |-> 0 ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      reason: Reasons,
      project: {"", "git-coupled", "missing"},
      project_id: 0..2,
      min_coupling: 0..100,
      min_commits: 0..MaxMinCommits,
      limit: 0..MaxLimit,
      candidate_pairs: SUBSET Pairs,
      result_count: 0..MaxLimit,
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

SuccessfulRequestsResolvedUniqueProject ==
    response.outcome = "ok" =>
        /\ response.project = "git-coupled"
        /\ response.project_id = 1
        /\ req.project_case = "unique"

ParamsBounded ==
    response.outcome \in {"ok", "no_data"} =>
        /\ response.min_coupling \in 0..100
        /\ response.min_commits \in 1..MaxMinCommits
        /\ response.limit \in 1..MaxLimit

CandidateRowsProjectScoped ==
    response.outcome = "ok" =>
        \A p \in response.candidate_pairs : PairProject(p) = response.project_id

BulkCommitsExcluded ==
    response.outcome = "ok" =>
        \A p \in response.candidate_pairs : ~PairBulk(p)

ThresholdsEnforced ==
    response.outcome = "ok" =>
        \A p \in response.candidate_pairs :
            /\ PairJaccard(p) >= response.min_coupling
            /\ PairCoCommits(p) >= response.min_commits

LimitBound ==
    response.outcome = "ok" => response.result_count <= response.limit

NoDataDoesNotQueryRows ==
    response.outcome = "no_data" =>
        /\ response.candidate_pairs = {}
        /\ response.result_count = 0

ReadOnlyNoLocks ==
    /\ response.writes = 0
    /\ response.locks = 0

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidRequestsReject /\
        SuccessfulRequestsResolvedUniqueProject /\
        ParamsBounded /\
        CandidateRowsProjectScoped /\
        BulkCommitsExcluded /\
        ThresholdsEnforced /\
        LimitBound /\
        NoDataDoesNotQueryRows /\
        ReadOnlyNoLocks)

=============================================================================
