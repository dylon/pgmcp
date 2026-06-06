------------------------ MODULE WorkItemSearchIngestBulk ------------------------
(***************************************************************************)
(* `work_item_search`, `work_item_ingest_plan`, and `work_item_bulk`        *)
(* request-boundary model.                                                  *)
(*                                                                         *)
(* The model abstracts each tool call into a normalized response: search     *)
(* validates scope and embedding dimensions before pgvector; ingestion       *)
(* rejects oversized plans before writes and commits plan nodes/criteria in  *)
(* one transaction; bulk normalizes explicit targets, validates operation    *)
(* inputs up front, and uses the agent-only per-item status chokepoint.      *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

NoLimit == -999
NoPriority == -999
ExpectedDim == 1024
MaxSearchLimit == 100
MaxIngestNodes == 3
MaxBulkTargets == 3

Modes == {"search", "ingest", "bulk"}
Projects == {"pgmcp"}
Ops == {"assign", "reprioritize", "set_status", "nope"}
Actors == {"agent", "none"}
Outcomes == {"ok", "rejected"}
Reasons ==
    {"none", "blank_query", "unknown_project", "bad_embedding_dim",
     "empty_plan", "unrecognized_plan", "plan_too_large", "db_failure",
     "no_targets", "blank_public", "unknown_op", "priority_required",
     "priority_bounds"}

Requests ==
    { [ id |-> 1, mode |-> "search", raw_query |-> " tracker ",
        raw_project |-> " pgmcp ", embed_dim |-> ExpectedDim, limit |-> 500,
        plan_nodes |-> 0, plan_recognized |-> TRUE, db_fails |-> FALSE,
        has_parent |-> FALSE, op |-> "assign", target_count |-> 0,
        blank_public |-> FALSE, duplicate_count |-> 0, priority |-> NoPriority,
        transition_failures |-> 0 ],
      [ id |-> 2, mode |-> "search", raw_query |-> "   ",
        raw_project |-> "", embed_dim |-> ExpectedDim, limit |-> NoLimit,
        plan_nodes |-> 0, plan_recognized |-> TRUE, db_fails |-> FALSE,
        has_parent |-> FALSE, op |-> "assign", target_count |-> 0,
        blank_public |-> FALSE, duplicate_count |-> 0, priority |-> NoPriority,
        transition_failures |-> 0 ],
      [ id |-> 3, mode |-> "search", raw_query |-> "tracker",
        raw_project |-> "missing-project", embed_dim |-> ExpectedDim,
        limit |-> 10, plan_nodes |-> 0, plan_recognized |-> TRUE,
        db_fails |-> FALSE, has_parent |-> FALSE, op |-> "assign",
        target_count |-> 0, blank_public |-> FALSE, duplicate_count |-> 0,
        priority |-> NoPriority, transition_failures |-> 0 ],
      [ id |-> 4, mode |-> "search", raw_query |-> "tracker",
        raw_project |-> "", embed_dim |-> 8, limit |-> 10,
        plan_nodes |-> 0, plan_recognized |-> TRUE, db_fails |-> FALSE,
        has_parent |-> FALSE, op |-> "assign", target_count |-> 0,
        blank_public |-> FALSE, duplicate_count |-> 0, priority |-> NoPriority,
        transition_failures |-> 0 ],

      [ id |-> 5, mode |-> "ingest", raw_query |-> "",
        raw_project |-> "", embed_dim |-> ExpectedDim, limit |-> NoLimit,
        plan_nodes |-> 2, plan_recognized |-> TRUE, db_fails |-> FALSE,
        has_parent |-> TRUE, op |-> "assign", target_count |-> 0,
        blank_public |-> FALSE, duplicate_count |-> 0, priority |-> NoPriority,
        transition_failures |-> 0 ],
      [ id |-> 6, mode |-> "ingest", raw_query |-> "",
        raw_project |-> "", embed_dim |-> ExpectedDim, limit |-> NoLimit,
        plan_nodes |-> 0, plan_recognized |-> TRUE, db_fails |-> FALSE,
        has_parent |-> FALSE, op |-> "assign", target_count |-> 0,
        blank_public |-> FALSE, duplicate_count |-> 0, priority |-> NoPriority,
        transition_failures |-> 0 ],
      [ id |-> 7, mode |-> "ingest", raw_query |-> "",
        raw_project |-> "", embed_dim |-> ExpectedDim, limit |-> NoLimit,
        plan_nodes |-> 1, plan_recognized |-> FALSE, db_fails |-> FALSE,
        has_parent |-> FALSE, op |-> "assign", target_count |-> 0,
        blank_public |-> FALSE, duplicate_count |-> 0, priority |-> NoPriority,
        transition_failures |-> 0 ],
      [ id |-> 8, mode |-> "ingest", raw_query |-> "",
        raw_project |-> "", embed_dim |-> ExpectedDim, limit |-> NoLimit,
        plan_nodes |-> MaxIngestNodes + 1, plan_recognized |-> TRUE,
        db_fails |-> FALSE, has_parent |-> TRUE, op |-> "assign",
        target_count |-> 0, blank_public |-> FALSE, duplicate_count |-> 0,
        priority |-> NoPriority, transition_failures |-> 0 ],
      [ id |-> 9, mode |-> "ingest", raw_query |-> "",
        raw_project |-> "missing-project", embed_dim |-> ExpectedDim,
        limit |-> NoLimit, plan_nodes |-> 2, plan_recognized |-> TRUE,
        db_fails |-> FALSE, has_parent |-> TRUE, op |-> "assign",
        target_count |-> 0, blank_public |-> FALSE, duplicate_count |-> 0,
        priority |-> NoPriority, transition_failures |-> 0 ],
      [ id |-> 10, mode |-> "ingest", raw_query |-> "",
        raw_project |-> "", embed_dim |-> ExpectedDim, limit |-> NoLimit,
        plan_nodes |-> 2, plan_recognized |-> TRUE, db_fails |-> TRUE,
        has_parent |-> TRUE, op |-> "assign", target_count |-> 0,
        blank_public |-> FALSE, duplicate_count |-> 0, priority |-> NoPriority,
        transition_failures |-> 0 ],

      [ id |-> 11, mode |-> "bulk", raw_query |-> "",
        raw_project |-> "", embed_dim |-> ExpectedDim, limit |-> NoLimit,
        plan_nodes |-> 0, plan_recognized |-> TRUE, db_fails |-> FALSE,
        has_parent |-> FALSE, op |-> "assign", target_count |-> 2,
        blank_public |-> FALSE, duplicate_count |-> 1, priority |-> NoPriority,
        transition_failures |-> 0 ],
      [ id |-> 12, mode |-> "bulk", raw_query |-> "",
        raw_project |-> "", embed_dim |-> ExpectedDim, limit |-> NoLimit,
        plan_nodes |-> 0, plan_recognized |-> TRUE, db_fails |-> FALSE,
        has_parent |-> FALSE, op |-> "assign", target_count |-> 0,
        blank_public |-> FALSE, duplicate_count |-> 0, priority |-> NoPriority,
        transition_failures |-> 0 ],
      [ id |-> 13, mode |-> "bulk", raw_query |-> "",
        raw_project |-> "", embed_dim |-> ExpectedDim, limit |-> NoLimit,
        plan_nodes |-> 0, plan_recognized |-> TRUE, db_fails |-> FALSE,
        has_parent |-> FALSE, op |-> "assign", target_count |-> 1,
        blank_public |-> TRUE, duplicate_count |-> 0, priority |-> NoPriority,
        transition_failures |-> 0 ],
      [ id |-> 14, mode |-> "bulk", raw_query |-> "",
        raw_project |-> "", embed_dim |-> ExpectedDim, limit |-> NoLimit,
        plan_nodes |-> 0, plan_recognized |-> TRUE, db_fails |-> FALSE,
        has_parent |-> FALSE, op |-> "nope", target_count |-> 1,
        blank_public |-> FALSE, duplicate_count |-> 0, priority |-> NoPriority,
        transition_failures |-> 0 ],
      [ id |-> 15, mode |-> "bulk", raw_query |-> "",
        raw_project |-> "", embed_dim |-> ExpectedDim, limit |-> NoLimit,
        plan_nodes |-> 0, plan_recognized |-> TRUE, db_fails |-> FALSE,
        has_parent |-> FALSE, op |-> "reprioritize", target_count |-> 1,
        blank_public |-> FALSE, duplicate_count |-> 0, priority |-> 101,
        transition_failures |-> 0 ],
      [ id |-> 16, mode |-> "bulk", raw_query |-> "",
        raw_project |-> "", embed_dim |-> ExpectedDim, limit |-> NoLimit,
        plan_nodes |-> 0, plan_recognized |-> TRUE, db_fails |-> FALSE,
        has_parent |-> FALSE, op |-> "reprioritize", target_count |-> 1,
        blank_public |-> FALSE, duplicate_count |-> 0, priority |-> 42,
        transition_failures |-> 0 ],
      [ id |-> 17, mode |-> "bulk", raw_query |-> "",
        raw_project |-> "", embed_dim |-> ExpectedDim, limit |-> NoLimit,
        plan_nodes |-> 0, plan_recognized |-> TRUE, db_fails |-> FALSE,
        has_parent |-> FALSE, op |-> "set_status", target_count |-> 2,
        blank_public |-> FALSE, duplicate_count |-> 0, priority |-> NoPriority,
        transition_failures |-> 1 ] }

RequestIds == {r.id : r \in Requests}

Normalize(raw) ==
    CASE raw = " tracker " -> "tracker"
      [] raw = " pgmcp " -> "pgmcp"
      [] raw = "   " -> ""
      [] OTHER -> raw

SearchLimit(r) ==
    IF r.limit = NoLimit THEN 10
    ELSE IF r.limit < 1 THEN 1
    ELSE IF r.limit > MaxSearchLimit THEN MaxSearchLimit
    ELSE r.limit

UniqueTargets(r) == r.target_count - r.duplicate_count

ReasonFor(r) ==
    LET query == Normalize(r.raw_query) IN
    LET project == Normalize(r.raw_project) IN
        CASE r.mode = "search" /\ query = "" -> "blank_query"
          [] r.mode = "search" /\ project # "" /\ ~(project \in Projects) -> "unknown_project"
          [] r.mode = "search" /\ r.embed_dim # ExpectedDim -> "bad_embedding_dim"
          [] r.mode = "ingest" /\ r.plan_nodes = 0 -> "empty_plan"
          [] r.mode = "ingest" /\ ~r.plan_recognized -> "unrecognized_plan"
          [] r.mode = "ingest" /\ r.plan_nodes > MaxIngestNodes -> "plan_too_large"
          [] r.mode = "ingest" /\ project # "" /\ ~(project \in Projects) -> "unknown_project"
          [] r.mode = "ingest" /\ r.db_fails -> "db_failure"
          [] r.mode = "bulk" /\ r.target_count = 0 -> "no_targets"
          [] r.mode = "bulk" /\ r.blank_public -> "blank_public"
          [] r.mode = "bulk" /\ r.op = "nope" -> "unknown_op"
          [] r.mode = "bulk" /\ r.op = "reprioritize" /\ r.priority = NoPriority -> "priority_required"
          [] r.mode = "bulk" /\ r.op = "reprioritize" /\ ~(r.priority \in 0..100) -> "priority_bounds"
          [] OTHER -> "none"

HitProjects(r) ==
    LET project == Normalize(r.raw_project) IN
        IF r.mode = "search" /\ ReasonFor(r) = "none"
        THEN IF project = "" THEN Projects ELSE {project}
        ELSE {}

ResponseFor(r) ==
    LET reason == ReasonFor(r) IN
    LET ok == reason = "none" IN
    LET attempted == IF r.mode = "bulk" /\ ok THEN UniqueTargets(r) ELSE 0 IN
    LET failed == IF r.mode = "bulk" /\ ok THEN r.transition_failures ELSE 0 IN
        [ request_id |-> r.id,
          mode |-> r.mode,
          outcome |-> IF ok THEN "ok" ELSE "rejected",
          reason |-> reason,
          project |-> Normalize(r.raw_project),
          search_limit |-> IF r.mode = "search" THEN SearchLimit(r) ELSE 0,
          hit_projects |-> HitProjects(r),
          items_written |-> IF r.mode = "ingest" /\ ok THEN r.plan_nodes ELSE 0,
          criteria_written |-> IF r.mode = "ingest" /\ ok THEN r.plan_nodes ELSE 0,
          parent_locked |-> r.mode = "ingest" /\ ok /\ r.has_parent,
          attempted |-> attempted,
          succeeded |-> IF r.mode = "bulk" /\ ok THEN attempted - failed ELSE 0,
          failed |-> failed,
          actor |-> IF r.mode = "bulk" /\ ok /\ r.op = "set_status" THEN "agent" ELSE "none" ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      mode: Modes,
      outcome: Outcomes,
      reason: Reasons,
      project: Projects \cup {"", "missing-project"},
      search_limit: 0..MaxSearchLimit,
      hit_projects: SUBSET Projects,
      items_written: 0..MaxIngestNodes,
      criteria_written: 0..MaxIngestNodes,
      parent_locked: BOOLEAN,
      attempted: 0..MaxBulkTargets,
      succeeded: 0..MaxBulkTargets,
      failed: 0..MaxBulkTargets,
      actor: Actors ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK == response \in ResponseRecord

SearchInvalidRejected ==
    req.mode = "search" /\ ReasonFor(req) # "none" =>
        response.outcome = "rejected"

SearchLimitBounded ==
    req.mode = "search" => response.search_limit \in 1..MaxSearchLimit

SearchEmbeddingDimGuard ==
    req.mode = "search" /\ response.outcome = "ok" =>
        req.embed_dim = ExpectedDim

SearchHitsScoped ==
    req.mode = "search" /\ response.outcome = "ok" =>
        response.hit_projects \subseteq
            (IF Normalize(req.raw_project) = "" THEN Projects ELSE {Normalize(req.raw_project)})

IngestInvalidOrFailedWritesNothing ==
    req.mode = "ingest" /\ response.outcome = "rejected" =>
        /\ response.items_written = 0
        /\ response.criteria_written = 0

IngestOversizeRejectedBeforeWrite ==
    req.mode = "ingest" /\ req.plan_nodes > MaxIngestNodes =>
        /\ response.reason = "plan_too_large"
        /\ response.items_written = 0
        /\ response.criteria_written = 0

IngestDbFailureRollsBack ==
    req.mode = "ingest" /\ req.db_fails =>
        /\ response.reason = "db_failure"
        /\ response.items_written = 0
        /\ response.criteria_written = 0

IngestAtomicWrites ==
    req.mode = "ingest" /\ response.outcome = "ok" =>
        /\ response.items_written = req.plan_nodes
        /\ response.criteria_written = req.plan_nodes

IngestParentRootLocked ==
    req.mode = "ingest" /\ response.outcome = "ok" /\ req.has_parent =>
        response.parent_locked

BulkInvalidRejectedBeforeMutation ==
    req.mode = "bulk" /\ ReasonFor(req) # "none" =>
        /\ response.outcome = "rejected"
        /\ response.attempted = 0
        /\ response.succeeded = 0
        /\ response.failed = 0

BulkDedupesTargets ==
    req.mode = "bulk" /\ response.outcome = "ok" =>
        response.attempted = UniqueTargets(req)

BulkPriorityBounds ==
    req.mode = "bulk" /\ response.outcome = "ok" /\ req.op = "reprioritize" =>
        req.priority \in 0..100

BulkStatusActorIsAgent ==
    req.mode = "bulk" /\ response.outcome = "ok" /\ req.op = "set_status" =>
        response.actor = "agent"

BulkPartialSuccessAccounting ==
    req.mode = "bulk" /\ response.outcome = "ok" =>
        response.succeeded + response.failed = response.attempted

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        SearchInvalidRejected /\
        SearchLimitBounded /\
        SearchEmbeddingDimGuard /\
        SearchHitsScoped /\
        IngestInvalidOrFailedWritesNothing /\
        IngestOversizeRejectedBeforeWrite /\
        IngestDbFailureRollsBack /\
        IngestAtomicWrites /\
        IngestParentRootLocked /\
        BulkInvalidRejectedBeforeMutation /\
        BulkDedupesTargets /\
        BulkPriorityBounds /\
        BulkStatusActorIsAgent /\
        BulkPartialSuccessAccounting)

=============================================================================
