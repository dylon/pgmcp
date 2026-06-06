-------------------------- MODULE WorkItemCreateAtomicity --------------------------
(***************************************************************************)
(* `work_item_create` request and transaction boundary.                    *)
(*                                                                         *)
(* The tool accepts agent-authored work-item creation requests. It must     *)
(* normalize text fields, reject invalid caller input before persistence,   *)
(* keep bug-only fields on first-class bugs, lock a parent before deriving  *)
(* the child root, and commit the item row plus bug sidecar atomically.     *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

NoPriority == -999
NoWeight == -999

Kinds == {"task", "bug", "plan"}
Severities == {"critical", "high", "medium", "low"}
Projects == {"pgmcp"}
Parents == {"PARENT"}
ExistingPublicIds == {"existing"}

Reasons ==
    {"none", "blank_title", "unknown_kind", "bug_field_on_non_bug",
     "unknown_severity", "priority_bounds", "weight_bounds",
     "unknown_project", "unknown_parent", "duplicate_public",
     "sidecar_failure"}

Outcomes == {"ok", "rejected"}
Statuses == {"none", "pending", "triage"}
RootRefs == {"none", "self", "parent_root"}
PublicIds == {"none", "generated", "normalized-id", "bug-id", "child-id", "existing"}
Titles == {"none", "Normalize create input", "bug title", "child"}

Requests ==
    { [ id |-> 1, raw_kind |-> " task ", raw_title |-> "  Normalize create input  ",
        raw_public |-> " normalized-id ", raw_project |-> "", raw_parent |-> "",
        raw_severity |-> "", raw_repro |-> "", priority |-> 100, weight |-> 1,
        sidecar_ok |-> TRUE ],
      [ id |-> 2, raw_kind |-> "task", raw_title |-> "   ",
        raw_public |-> "", raw_project |-> "", raw_parent |-> "",
        raw_severity |-> "", raw_repro |-> "", priority |-> NoPriority, weight |-> NoWeight,
        sidecar_ok |-> TRUE ],
      [ id |-> 3, raw_kind |-> "done", raw_title |-> "child",
        raw_public |-> "", raw_project |-> "", raw_parent |-> "",
        raw_severity |-> "", raw_repro |-> "", priority |-> NoPriority, weight |-> NoWeight,
        sidecar_ok |-> TRUE ],
      [ id |-> 4, raw_kind |-> "task", raw_title |-> "child",
        raw_public |-> "", raw_project |-> "", raw_parent |-> "",
        raw_severity |-> "low", raw_repro |-> "", priority |-> NoPriority, weight |-> NoWeight,
        sidecar_ok |-> TRUE ],
      [ id |-> 5, raw_kind |-> "task", raw_title |-> "child",
        raw_public |-> "", raw_project |-> "", raw_parent |-> "",
        raw_severity |-> "", raw_repro |-> "do X", priority |-> NoPriority, weight |-> NoWeight,
        sidecar_ok |-> TRUE ],
      [ id |-> 6, raw_kind |-> "bug", raw_title |-> "bug title",
        raw_public |-> "bug-id", raw_project |-> " pgmcp ", raw_parent |-> "",
        raw_severity |-> " high ", raw_repro |-> "do X", priority |-> NoPriority, weight |-> NoWeight,
        sidecar_ok |-> TRUE ],
      [ id |-> 7, raw_kind |-> "bug", raw_title |-> "bug title",
        raw_public |-> "", raw_project |-> "", raw_parent |-> "",
        raw_severity |-> "apocalyptic", raw_repro |-> "", priority |-> NoPriority, weight |-> NoWeight,
        sidecar_ok |-> TRUE ],
      [ id |-> 8, raw_kind |-> "task", raw_title |-> "child",
        raw_public |-> "", raw_project |-> "", raw_parent |-> "",
        raw_severity |-> "", raw_repro |-> "", priority |-> 101, weight |-> NoWeight,
        sidecar_ok |-> TRUE ],
      [ id |-> 9, raw_kind |-> "task", raw_title |-> "child",
        raw_public |-> "", raw_project |-> "", raw_parent |-> "",
        raw_severity |-> "", raw_repro |-> "", priority |-> NoPriority, weight |-> 0,
        sidecar_ok |-> TRUE ],
      [ id |-> 10, raw_kind |-> "task", raw_title |-> "child",
        raw_public |-> "", raw_project |-> "missing-project", raw_parent |-> "",
        raw_severity |-> "", raw_repro |-> "", priority |-> NoPriority, weight |-> NoWeight,
        sidecar_ok |-> TRUE ],
      [ id |-> 11, raw_kind |-> "task", raw_title |-> "child",
        raw_public |-> "existing", raw_project |-> "", raw_parent |-> "",
        raw_severity |-> "", raw_repro |-> "", priority |-> NoPriority, weight |-> NoWeight,
        sidecar_ok |-> TRUE ],
      [ id |-> 12, raw_kind |-> "bug", raw_title |-> "bug title",
        raw_public |-> "bug-id", raw_project |-> "", raw_parent |-> "",
        raw_severity |-> "high", raw_repro |-> "do X", priority |-> NoPriority, weight |-> NoWeight,
        sidecar_ok |-> FALSE ],
      [ id |-> 13, raw_kind |-> "task", raw_title |-> "child",
        raw_public |-> "child-id", raw_project |-> "", raw_parent |-> " PARENT ",
        raw_severity |-> "", raw_repro |-> "", priority |-> NoPriority, weight |-> NoWeight,
        sidecar_ok |-> TRUE ],
      [ id |-> 14, raw_kind |-> "task", raw_title |-> "child",
        raw_public |-> "", raw_project |-> "", raw_parent |-> "missing-parent",
        raw_severity |-> "", raw_repro |-> "", priority |-> NoPriority, weight |-> NoWeight,
        sidecar_ok |-> TRUE ] }

RequestIds == {r.id : r \in Requests}

NormalizeKind(raw) ==
    CASE raw = " task " -> "task"
      [] OTHER -> raw

NormalizeTitle(raw) ==
    CASE raw = "  Normalize create input  " -> "Normalize create input"
      [] raw = "   " -> ""
      [] OTHER -> raw

NormalizePublic(raw) ==
    CASE raw = " normalized-id " -> "normalized-id"
      [] raw = "" -> "generated"
      [] raw = "   " -> "generated"
      [] OTHER -> raw

NormalizeProject(raw) ==
    CASE raw = " pgmcp " -> "pgmcp"
      [] raw = "" -> "none"
      [] raw = "   " -> "none"
      [] OTHER -> raw

NormalizeParent(raw) ==
    CASE raw = " PARENT " -> "PARENT"
      [] raw = "" -> "none"
      [] raw = "   " -> "none"
      [] OTHER -> raw

NormalizeSeverity(raw) ==
    CASE raw = " high " -> "high"
      [] raw = "" -> "none"
      [] raw = "   " -> "none"
      [] OTHER -> raw

HasBugText(r) == r.raw_repro # "" /\ r.raw_repro # "   "
HasSeverity(r) == NormalizeSeverity(r.raw_severity) # "none"
BugFieldsSupplied(r) == HasSeverity(r) \/ HasBugText(r)

SeverityPriority(sev) ==
    CASE sev = "critical" -> 90
      [] sev = "high" -> 70
      [] sev = "medium" -> 40
      [] sev = "low" -> 20
      [] OTHER -> 0

PriorityFor(r) ==
    IF r.priority # NoPriority THEN r.priority
    ELSE SeverityPriority(NormalizeSeverity(r.raw_severity))

WeightFor(r) == IF r.weight = NoWeight THEN 1 ELSE r.weight

RejectReason(r) ==
    LET kind == NormalizeKind(r.raw_kind) IN
    LET title == NormalizeTitle(r.raw_title) IN
    LET project == NormalizeProject(r.raw_project) IN
    LET parent == NormalizeParent(r.raw_parent) IN
    LET severity == NormalizeSeverity(r.raw_severity) IN
    LET public == NormalizePublic(r.raw_public) IN
        CASE title = "" -> "blank_title"
          [] ~(kind \in Kinds) -> "unknown_kind"
          [] kind # "bug" /\ BugFieldsSupplied(r) -> "bug_field_on_non_bug"
          [] severity # "none" /\ ~(severity \in Severities) -> "unknown_severity"
          [] ~(PriorityFor(r) \in 0..100) -> "priority_bounds"
          [] WeightFor(r) <= 0 -> "weight_bounds"
          [] project # "none" /\ ~(project \in Projects) -> "unknown_project"
          [] parent # "none" /\ ~(parent \in Parents) -> "unknown_parent"
          [] public \in ExistingPublicIds -> "duplicate_public"
          [] kind = "bug" /\ ~r.sidecar_ok -> "sidecar_failure"
          [] OTHER -> "none"

NoItem ==
    [ exists |-> FALSE, public_id |-> "none", kind |-> "none",
      status |-> "none", title |-> "none", priority |-> 0,
      weight |-> 1, severity |-> "none", parent |-> "none",
      root |-> "none" ]

NoBugDetails == [ exists |-> FALSE, item_public_id |-> "none", has_repro |-> FALSE ]

ItemFor(r) ==
    LET kind == NormalizeKind(r.raw_kind) IN
    LET parent == NormalizeParent(r.raw_parent) IN
        [ exists |-> TRUE,
          public_id |-> NormalizePublic(r.raw_public),
          kind |-> kind,
          status |-> IF kind = "bug" THEN "triage" ELSE "pending",
          title |-> NormalizeTitle(r.raw_title),
          priority |-> PriorityFor(r),
          weight |-> WeightFor(r),
          severity |-> IF kind = "bug" THEN NormalizeSeverity(r.raw_severity) ELSE "none",
          parent |-> parent,
          root |-> IF parent = "none" THEN "none" ELSE "parent_root" ]

BugDetailsFor(r) ==
    [ exists |-> TRUE,
      item_public_id |-> NormalizePublic(r.raw_public),
      has_repro |-> HasBugText(r) ]

ResponseFor(r) ==
    LET reason == RejectReason(r) IN
    LET kind == NormalizeKind(r.raw_kind) IN
        IF reason # "none" THEN
            [ request_id |-> r.id,
              outcome |-> "rejected",
              reason |-> reason,
              item |-> NoItem,
              bug_details |-> NoBugDetails,
              parent_locked |-> FALSE ]
        ELSE
            [ request_id |-> r.id,
              outcome |-> "ok",
              reason |-> "none",
              item |-> ItemFor(r),
              bug_details |-> IF kind = "bug" THEN BugDetailsFor(r) ELSE NoBugDetails,
              parent_locked |-> NormalizeParent(r.raw_parent) # "none" ]

VARIABLES req, response

vars == <<req, response>>

ItemRecord ==
    [ exists: BOOLEAN,
      public_id: PublicIds,
      kind: Kinds \cup {"none"},
      status: Statuses,
      title: Titles,
      priority: 0..100,
      weight: {1},
      severity: Severities \cup {"none"},
      parent: Parents \cup {"none"},
      root: RootRefs ]

BugDetailsRecord ==
    [ exists: BOOLEAN,
      item_public_id: PublicIds,
      has_repro: BOOLEAN ]

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      reason: Reasons,
      item: ItemRecord,
      bug_details: BugDetailsRecord,
      parent_locked: BOOLEAN ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests
    /\ response \in ResponseRecord

RejectedWritesNothing ==
    response.outcome = "rejected" =>
        /\ response.item.exists = FALSE
        /\ response.bug_details.exists = FALSE

AcceptedWritesOneItem ==
    response.outcome = "ok" => response.item.exists = TRUE

BugSidecarAtomic ==
    NormalizeKind(req.raw_kind) = "bug" =>
        IF response.outcome = "ok"
        THEN /\ response.item.exists = TRUE
             /\ response.bug_details.exists = TRUE
             /\ response.bug_details.item_public_id = response.item.public_id
        ELSE /\ response.item.exists = FALSE
             /\ response.bug_details.exists = FALSE

NonBugNeverHasSeverityOrSidecar ==
    response.outcome = "ok" /\ response.item.kind # "bug" =>
        /\ response.item.severity = "none"
        /\ response.bug_details.exists = FALSE

BugOnlyFieldsRequireBug ==
    NormalizeKind(req.raw_kind) # "bug" /\ BugFieldsSupplied(req) =>
        response.reason = "bug_field_on_non_bug"

StatusByKind ==
    response.outcome = "ok" =>
        IF response.item.kind = "bug"
        THEN response.item.status = "triage"
        ELSE response.item.status = "pending"

PriorityAndWeightBounded ==
    response.outcome = "ok" =>
        /\ response.item.priority \in 0..100
        /\ response.item.weight > 0

UnknownProjectRejected ==
    LET project == NormalizeProject(req.raw_project) IN
        project # "none" /\ ~(project \in Projects) =>
            response.reason = "unknown_project"

DuplicatePublicRejected ==
    NormalizePublic(req.raw_public) \in ExistingPublicIds =>
        response.reason = "duplicate_public"

ParentLockedBeforeChildInsert ==
    response.outcome = "ok" /\ response.item.parent # "none" =>
        /\ response.parent_locked = TRUE
        /\ response.item.root = "parent_root"

SidecarFailureRollsBack ==
    NormalizeKind(req.raw_kind) = "bug" /\ ~req.sidecar_ok =>
        /\ response.reason = "sidecar_failure"
        /\ response.item.exists = FALSE
        /\ response.bug_details.exists = FALSE

NormalizedCreateFields ==
    response.outcome = "ok" =>
        /\ response.item.kind = NormalizeKind(req.raw_kind)
        /\ response.item.title = NormalizeTitle(req.raw_title)
        /\ response.item.public_id = NormalizePublic(req.raw_public)

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        RejectedWritesNothing /\
        AcceptedWritesOneItem /\
        BugSidecarAtomic /\
        NonBugNeverHasSeverityOrSidecar /\
        BugOnlyFieldsRequireBug /\
        StatusByKind /\
        PriorityAndWeightBounded /\
        UnknownProjectRejected /\
        DuplicatePublicRejected /\
        ParentLockedBeforeChildInsert /\
        SidecarFailureRollsBack /\
        NormalizedCreateFields)

=============================================================================
