--------------------------- MODULE WorkItemCrudReadUpdate ---------------------------
(***************************************************************************)
(* `work_item_get`, `work_item_list`, and `work_item_update` request        *)
(* boundary.                                                               *)
(*                                                                         *)
(* Reads normalize identifiers and fail closed. List filters validate       *)
(* project/kind/status before querying. Update validates mutable fields and *)
(* commits the row update and bug sidecar atomically.                       *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

NoPriority == -999
NoWeight == -999
NoLimit == -999

Modes == {"get", "list", "update"}
Kinds == {"task", "bug"}
Statuses == {"pending", "triage", "in_progress"}
Projects == {"pgmcp"}
PublicIds == {"", "none", "task-1", "bug-1", "missing"}
Outcomes == {"ok", "rejected"}
Reasons ==
    {"none", "blank_public", "not_found", "unknown_project",
     "unknown_kind", "unknown_status", "blank_title", "priority_bounds",
     "weight_bounds", "bug_field_on_non_bug", "sidecar_failure"}
ProjectResponses == Projects \cup {"", "missing-project"}
KindFilterResponses == Kinds \cup {"", "nope"}
StatusFilterResponses == Statuses \cup {"", "nope"}

Requests ==
    { [ id |-> 1, mode |-> "get", raw_public |-> " task-1 ", raw_project |-> "",
        raw_kind |-> "", raw_status |-> "", raw_title |-> "", priority |-> NoPriority,
        weight |-> NoWeight, bug_fields |-> FALSE, target_kind |-> "task",
        sidecar_ok |-> TRUE, limit |-> NoLimit ],
      [ id |-> 2, mode |-> "get", raw_public |-> "   ", raw_project |-> "",
        raw_kind |-> "", raw_status |-> "", raw_title |-> "", priority |-> NoPriority,
        weight |-> NoWeight, bug_fields |-> FALSE, target_kind |-> "task",
        sidecar_ok |-> TRUE, limit |-> NoLimit ],
      [ id |-> 3, mode |-> "get", raw_public |-> "missing", raw_project |-> "",
        raw_kind |-> "", raw_status |-> "", raw_title |-> "", priority |-> NoPriority,
        weight |-> NoWeight, bug_fields |-> FALSE, target_kind |-> "task",
        sidecar_ok |-> TRUE, limit |-> NoLimit ],
      [ id |-> 4, mode |-> "list", raw_public |-> "", raw_project |-> " pgmcp ",
        raw_kind |-> " task ", raw_status |-> " pending ", raw_title |-> "",
        priority |-> NoPriority, weight |-> NoWeight, bug_fields |-> FALSE,
        target_kind |-> "task", sidecar_ok |-> TRUE, limit |-> 2000 ],
      [ id |-> 5, mode |-> "list", raw_public |-> "", raw_project |-> "missing-project",
        raw_kind |-> "", raw_status |-> "", raw_title |-> "", priority |-> NoPriority,
        weight |-> NoWeight, bug_fields |-> FALSE, target_kind |-> "task",
        sidecar_ok |-> TRUE, limit |-> NoLimit ],
      [ id |-> 6, mode |-> "list", raw_public |-> "", raw_project |-> "",
        raw_kind |-> "nope", raw_status |-> "", raw_title |-> "", priority |-> NoPriority,
        weight |-> NoWeight, bug_fields |-> FALSE, target_kind |-> "task",
        sidecar_ok |-> TRUE, limit |-> NoLimit ],
      [ id |-> 7, mode |-> "list", raw_public |-> "", raw_project |-> "",
        raw_kind |-> "", raw_status |-> "nope", raw_title |-> "", priority |-> NoPriority,
        weight |-> NoWeight, bug_fields |-> FALSE, target_kind |-> "task",
        sidecar_ok |-> TRUE, limit |-> NoLimit ],
      [ id |-> 8, mode |-> "update", raw_public |-> " task-1 ", raw_project |-> "",
        raw_kind |-> "", raw_status |-> "", raw_title |-> "  renamed  ", priority |-> 100,
        weight |-> 1, bug_fields |-> FALSE, target_kind |-> "task",
        sidecar_ok |-> TRUE, limit |-> NoLimit ],
      [ id |-> 9, mode |-> "update", raw_public |-> "task-1", raw_project |-> "",
        raw_kind |-> "", raw_status |-> "", raw_title |-> "   ", priority |-> NoPriority,
        weight |-> NoWeight, bug_fields |-> FALSE, target_kind |-> "task",
        sidecar_ok |-> TRUE, limit |-> NoLimit ],
      [ id |-> 10, mode |-> "update", raw_public |-> "task-1", raw_project |-> "",
        raw_kind |-> "", raw_status |-> "", raw_title |-> "", priority |-> 101,
        weight |-> NoWeight, bug_fields |-> FALSE, target_kind |-> "task",
        sidecar_ok |-> TRUE, limit |-> NoLimit ],
      [ id |-> 11, mode |-> "update", raw_public |-> "task-1", raw_project |-> "",
        raw_kind |-> "", raw_status |-> "", raw_title |-> "", priority |-> NoPriority,
        weight |-> 0, bug_fields |-> FALSE, target_kind |-> "task",
        sidecar_ok |-> TRUE, limit |-> NoLimit ],
      [ id |-> 12, mode |-> "update", raw_public |-> "task-1", raw_project |-> "",
        raw_kind |-> "", raw_status |-> "", raw_title |-> "", priority |-> NoPriority,
        weight |-> NoWeight, bug_fields |-> TRUE, target_kind |-> "task",
        sidecar_ok |-> TRUE, limit |-> NoLimit ],
      [ id |-> 13, mode |-> "update", raw_public |-> "bug-1", raw_project |-> "",
        raw_kind |-> "", raw_status |-> "", raw_title |-> "bug title", priority |-> NoPriority,
        weight |-> NoWeight, bug_fields |-> TRUE, target_kind |-> "bug",
        sidecar_ok |-> TRUE, limit |-> NoLimit ],
      [ id |-> 14, mode |-> "update", raw_public |-> "bug-1", raw_project |-> "",
        raw_kind |-> "", raw_status |-> "", raw_title |-> "bug title", priority |-> NoPriority,
        weight |-> NoWeight, bug_fields |-> TRUE, target_kind |-> "bug",
        sidecar_ok |-> FALSE, limit |-> NoLimit ] }

RequestIds == {r.id : r \in Requests}

Normalize(raw) ==
    CASE raw = " task-1 " -> "task-1"
      [] raw = " pgmcp " -> "pgmcp"
      [] raw = " task " -> "task"
      [] raw = " pending " -> "pending"
      [] raw = "  renamed  " -> "renamed"
      [] raw = "   " -> ""
      [] OTHER -> raw

KnownPublic(p) == p \in {"task-1", "bug-1"}

LimitFor(r) ==
    IF r.limit = NoLimit THEN 50
    ELSE IF r.limit < 1 THEN 1
    ELSE IF r.limit > 1000 THEN 1000
    ELSE r.limit

ReasonFor(r) ==
    LET public == Normalize(r.raw_public) IN
    LET project == Normalize(r.raw_project) IN
    LET kind == Normalize(r.raw_kind) IN
    LET status == Normalize(r.raw_status) IN
    LET title == Normalize(r.raw_title) IN
        CASE r.mode \in {"get", "update"} /\ public = "" -> "blank_public"
          [] r.mode \in {"get", "update"} /\ ~KnownPublic(public) -> "not_found"
          [] r.mode = "list" /\ project # "" /\ ~(project \in Projects) -> "unknown_project"
          [] r.mode = "list" /\ kind # "" /\ ~(kind \in Kinds) -> "unknown_kind"
          [] r.mode = "list" /\ status # "" /\ ~(status \in Statuses) -> "unknown_status"
          [] r.mode = "update" /\ r.raw_title # "" /\ title = "" -> "blank_title"
          [] r.mode = "update" /\ r.priority # NoPriority /\ ~(r.priority \in 0..100) -> "priority_bounds"
          [] r.mode = "update" /\ r.weight # NoWeight /\ r.weight <= 0 -> "weight_bounds"
          [] r.mode = "update" /\ r.target_kind # "bug" /\ r.bug_fields -> "bug_field_on_non_bug"
          [] r.mode = "update" /\ r.target_kind = "bug" /\ r.bug_fields /\ ~r.sidecar_ok -> "sidecar_failure"
          [] OTHER -> "none"

ResponseFor(r) ==
    LET reason == ReasonFor(r) IN
    LET updated == r.mode = "update" /\ reason = "none" IN
    LET sidecar == updated /\ r.target_kind = "bug" /\ r.bug_fields IN
        [ request_id |-> r.id,
          mode |-> r.mode,
          outcome |-> IF reason = "none" THEN "ok" ELSE "rejected",
          reason |-> reason,
          public_id |-> IF r.mode \in {"get", "update"} THEN Normalize(r.raw_public) ELSE "none",
          project |-> IF r.mode = "list" THEN Normalize(r.raw_project) ELSE "",
          kind_filter |-> IF r.mode = "list" THEN Normalize(r.raw_kind) ELSE "",
          status_filter |-> IF r.mode = "list" THEN Normalize(r.raw_status) ELSE "",
          limit |-> LimitFor(r),
          row_updated |-> updated,
          sidecar_updated |-> sidecar ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      mode: Modes,
      outcome: Outcomes,
      reason: Reasons,
      public_id: PublicIds,
      project: ProjectResponses,
      kind_filter: KindFilterResponses,
      status_filter: StatusFilterResponses,
      limit: 1..1000,
      row_updated: BOOLEAN,
      sidecar_updated: BOOLEAN ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests
    /\ response \in ResponseRecord

BlankPublicRejected ==
    req.mode \in {"get", "update"} /\ Normalize(req.raw_public) = "" =>
        response.reason = "blank_public"

ListFiltersValidated ==
    req.mode = "list" /\ response.outcome = "ok" =>
        /\ response.project \in Projects \cup {""}
        /\ response.kind_filter \in Kinds \cup {""}
        /\ response.status_filter \in Statuses \cup {""}

UnknownListFiltersFailClosed ==
    req.mode = "list" /\ ReasonFor(req) # "none" =>
        response.outcome = "rejected"

ListLimitClamped ==
    req.mode = "list" => response.limit \in 1..1000

UpdateBoundsChecked ==
    req.mode = "update" /\ response.outcome = "ok" =>
        /\ (req.priority = NoPriority \/ req.priority \in 0..100)
        /\ (req.weight = NoWeight \/ req.weight > 0)

BugFieldsOnlyOnBugs ==
    req.mode = "update" /\ req.bug_fields /\ req.target_kind # "bug" =>
        response.reason = "bug_field_on_non_bug"

UpdateAndSidecarAtomic ==
    req.mode = "update" /\ req.target_kind = "bug" /\ req.bug_fields =>
        IF response.outcome = "ok"
        THEN /\ response.row_updated = TRUE
             /\ response.sidecar_updated = TRUE
        ELSE /\ response.row_updated = FALSE
             /\ response.sidecar_updated = FALSE

SidecarFailureRollsBackUpdate ==
    req.mode = "update" /\ req.target_kind = "bug" /\ req.bug_fields /\ ~req.sidecar_ok =>
        /\ response.reason = "sidecar_failure"
        /\ response.row_updated = FALSE
        /\ response.sidecar_updated = FALSE

NormalizedPublicStored ==
    req.mode \in {"get", "update"} /\ response.outcome = "ok" =>
        response.public_id = Normalize(req.raw_public)

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        BlankPublicRejected /\
        ListFiltersValidated /\
        UnknownListFiltersFailClosed /\
        ListLimitClamped /\
        UpdateBoundsChecked /\
        BugFieldsOnlyOnBugs /\
        UpdateAndSidecarAtomic /\
        SidecarFailureRollsBackUpdate /\
        NormalizedPublicStored)

=============================================================================
