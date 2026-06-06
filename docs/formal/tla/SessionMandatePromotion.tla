--------------------------- MODULE SessionMandatePromotion ---------------------------
(***************************************************************************)
(* `session_mandates` and `promote_session_mandate` boundary model.        *)
(*                                                                         *)
(* The read tool must keep `cwd` scoped for every status filter.  The       *)
(* promotion tool validates scope/project/file inputs before mutation,      *)
(* serializes source-row promotion, returns an existing durable mandate on  *)
(* replay, and uses a single non-blocking file lock for optional appends.   *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

MaxLimit == 5

Kinds == {"read", "promote"}
Sessions == {"none", "a", "b"}
Cwds == {"none", "a", "b"}
Statuses == {"active", "promoted", "retired", "superseded", "all", "deleted"}
ValidStatuses == {"active", "promoted", "retired", "superseded", "all"}
Scopes == {"none", "project", "workspace", "bad"}
TargetModes == {"none", "missing", "blank", "agents", "long"}
FileLocks == {"none", "free", "busy"}
Outcomes == {"ok", "rejected"}
Reasons ==
    {"none", "missing_selector", "invalid_status", "invalid_scope",
     "invalid_project", "invalid_target", "file_busy", "inactive_source",
     "already_promoted_different_target"}

Rows ==
    { [session |-> "a", cwd |-> "a", status |-> "active"],
      [session |-> "b", cwd |-> "b", status |-> "promoted"] }

Requests ==
    { [ id |-> 1, kind |-> "read", session |-> "none", cwd |-> "a",
        status |-> "all", raw_limit |-> 0, scope |-> "none",
        project_id |-> 0, write_file |-> FALSE, target_mode |-> "none",
        file_lock |-> "none", source_status |-> "active",
        existing |-> FALSE, existing_scope |-> "none",
        existing_project |-> 0, existing_target |-> "none" ],
      [ id |-> 2, kind |-> "read", session |-> "none", cwd |-> "a",
        status |-> "deleted", raw_limit |-> 20, scope |-> "none",
        project_id |-> 0, write_file |-> FALSE, target_mode |-> "none",
        file_lock |-> "none", source_status |-> "active",
        existing |-> FALSE, existing_scope |-> "none",
        existing_project |-> 0, existing_target |-> "none" ],
      [ id |-> 3, kind |-> "read", session |-> "none", cwd |-> "none",
        status |-> "active", raw_limit |-> 20, scope |-> "none",
        project_id |-> 0, write_file |-> FALSE, target_mode |-> "none",
        file_lock |-> "none", source_status |-> "active",
        existing |-> FALSE, existing_scope |-> "none",
        existing_project |-> 0, existing_target |-> "none" ],
      [ id |-> 4, kind |-> "promote", session |-> "none", cwd |-> "none",
        status |-> "active", raw_limit |-> 20, scope |-> "workspace",
        project_id |-> 0, write_file |-> FALSE, target_mode |-> "none",
        file_lock |-> "none", source_status |-> "active",
        existing |-> FALSE, existing_scope |-> "none",
        existing_project |-> 0, existing_target |-> "none" ],
      [ id |-> 5, kind |-> "promote", session |-> "none", cwd |-> "none",
        status |-> "active", raw_limit |-> 20, scope |-> "workspace",
        project_id |-> 0, write_file |-> FALSE, target_mode |-> "none",
        file_lock |-> "none", source_status |-> "promoted",
        existing |-> TRUE, existing_scope |-> "workspace",
        existing_project |-> 0, existing_target |-> "none" ],
      [ id |-> 6, kind |-> "promote", session |-> "none", cwd |-> "none",
        status |-> "active", raw_limit |-> 20, scope |-> "workspace",
        project_id |-> 0, write_file |-> FALSE, target_mode |-> "none",
        file_lock |-> "none", source_status |-> "promoted",
        existing |-> TRUE, existing_scope |-> "project",
        existing_project |-> 7, existing_target |-> "none" ],
      [ id |-> 7, kind |-> "promote", session |-> "none", cwd |-> "none",
        status |-> "active", raw_limit |-> 20, scope |-> "project",
        project_id |-> 0, write_file |-> FALSE, target_mode |-> "none",
        file_lock |-> "none", source_status |-> "active",
        existing |-> FALSE, existing_scope |-> "none",
        existing_project |-> 0, existing_target |-> "none" ],
      [ id |-> 8, kind |-> "promote", session |-> "none", cwd |-> "none",
        status |-> "active", raw_limit |-> 20, scope |-> "workspace",
        project_id |-> 1, write_file |-> FALSE, target_mode |-> "none",
        file_lock |-> "none", source_status |-> "active",
        existing |-> FALSE, existing_scope |-> "none",
        existing_project |-> 0, existing_target |-> "none" ],
      [ id |-> 9, kind |-> "promote", session |-> "none", cwd |-> "none",
        status |-> "active", raw_limit |-> 20, scope |-> "workspace",
        project_id |-> 0, write_file |-> TRUE, target_mode |-> "missing",
        file_lock |-> "none", source_status |-> "active",
        existing |-> FALSE, existing_scope |-> "none",
        existing_project |-> 0, existing_target |-> "none" ],
      [ id |-> 10, kind |-> "promote", session |-> "none", cwd |-> "none",
        status |-> "active", raw_limit |-> 20, scope |-> "workspace",
        project_id |-> 0, write_file |-> TRUE, target_mode |-> "agents",
        file_lock |-> "busy", source_status |-> "active",
        existing |-> FALSE, existing_scope |-> "none",
        existing_project |-> 0, existing_target |-> "none" ],
      [ id |-> 11, kind |-> "promote", session |-> "none", cwd |-> "none",
        status |-> "active", raw_limit |-> 20, scope |-> "workspace",
        project_id |-> 0, write_file |-> TRUE, target_mode |-> "agents",
        file_lock |-> "free", source_status |-> "active",
        existing |-> FALSE, existing_scope |-> "none",
        existing_project |-> 0, existing_target |-> "none" ],
      [ id |-> 12, kind |-> "promote", session |-> "none", cwd |-> "none",
        status |-> "active", raw_limit |-> 20, scope |-> "workspace",
        project_id |-> 0, write_file |-> FALSE, target_mode |-> "none",
        file_lock |-> "none", source_status |-> "retired",
        existing |-> FALSE, existing_scope |-> "none",
        existing_project |-> 0, existing_target |-> "none" ] }

RequestIds == {r.id : r \in Requests}

Clamp(v, lo, hi) ==
    IF v < lo THEN lo ELSE IF v > hi THEN hi ELSE v

Min(a, b) == IF a <= b THEN a ELSE b

ReadReasonFor(r) ==
    CASE r.session = "none" /\ r.cwd = "none" -> "missing_selector"
      [] r.status \notin ValidStatuses -> "invalid_status"
      [] OTHER -> "none"

RowsFor(r) ==
    IF ReadReasonFor(r) # "none" THEN {}
    ELSE {row \in Rows :
        /\ (IF r.session # "none" THEN row.session = r.session ELSE row.cwd = r.cwd)
        /\ (r.status = "all" \/ row.status = r.status)}

TargetFor(r) ==
    CASE r.target_mode = "agents" -> "agents"
      [] OTHER -> "none"

PreLockPromotionReasonFor(r) ==
    CASE r.scope \notin {"project", "workspace"} -> "invalid_scope"
      [] r.scope = "project" /\ r.project_id <= 0 -> "invalid_project"
      [] r.scope = "workspace" /\ r.project_id # 0 -> "invalid_project"
      [] r.write_file /\ r.target_mode \in {"missing", "blank", "long"} -> "invalid_target"
      [] OTHER -> "none"

FileLockAttemptedFor(r) ==
    /\ r.kind = "promote"
    /\ r.write_file
    /\ PreLockPromotionReasonFor(r) = "none"

FileLockAcquiredFor(r) ==
    /\ FileLockAttemptedFor(r)
    /\ r.file_lock = "free"

PromotionReasonFor(r) ==
    LET pre == PreLockPromotionReasonFor(r) IN
    CASE pre # "none" -> pre
      [] FileLockAttemptedFor(r) /\ r.file_lock = "busy" -> "file_busy"
      [] r.existing /\
         (r.existing_scope # r.scope \/
          r.existing_project # r.project_id \/
          r.existing_target # TargetFor(r)) -> "already_promoted_different_target"
      [] ~r.existing /\ r.source_status # "active" -> "inactive_source"
      [] OTHER -> "none"

ReasonFor(r) ==
    IF r.kind = "read" THEN ReadReasonFor(r) ELSE PromotionReasonFor(r)

ReadResponseFor(r) ==
    LET reason == ReadReasonFor(r) IN
    LET limit == Clamp(r.raw_limit, 1, MaxLimit) IN
        [ request_id |-> r.id,
          outcome |-> IF reason = "none" THEN "ok" ELSE "rejected",
          reason |-> reason,
          limit |-> IF reason = "none" THEN limit ELSE 0,
          result_cwds |-> {row.cwd : row \in RowsFor(r)},
          result_sessions |-> {row.session : row \in RowsFor(r)},
          read_count |-> IF reason = "none" THEN Min(Cardinality(RowsFor(r)), limit) ELSE 0,
          durable_count |-> 0,
          inserted_durable |-> FALSE,
          db_writes |-> 0,
          file_lock_attempted |-> FALSE,
          file_lock_acquired |-> FALSE,
          file_writes |-> 0,
          lock_held_end |-> FALSE ]

PromoteResponseFor(r) ==
    LET reason == PromotionReasonFor(r) IN
    LET ok == reason = "none" IN
        [ request_id |-> r.id,
          outcome |-> IF ok THEN "ok" ELSE "rejected",
          reason |-> reason,
          limit |-> 0,
          result_cwds |-> {},
          result_sessions |-> {},
          read_count |-> 0,
          durable_count |-> IF ok \/ r.existing THEN 1 ELSE 0,
          inserted_durable |-> ok /\ ~r.existing,
          db_writes |-> IF ok THEN IF r.existing THEN 1 ELSE 2 ELSE 0,
          file_lock_attempted |-> FileLockAttemptedFor(r),
          file_lock_acquired |-> FileLockAcquiredFor(r),
          file_writes |-> IF ok /\ r.write_file THEN 1 ELSE 0,
          lock_held_end |-> FALSE ]

ResponseFor(r) ==
    IF r.kind = "read" THEN ReadResponseFor(r) ELSE PromoteResponseFor(r)

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      reason: Reasons,
      limit: 0..MaxLimit,
      result_cwds: SUBSET Cwds,
      result_sessions: SUBSET Sessions,
      read_count: 0..MaxLimit,
      durable_count: 0..1,
      inserted_durable: BOOLEAN,
      db_writes: 0..2,
      file_lock_attempted: BOOLEAN,
      file_lock_acquired: BOOLEAN,
      file_writes: 0..1,
      lock_held_end: BOOLEAN ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK == response \in ResponseRecord

InvalidReadsReject ==
    req.kind = "read" /\ ReadReasonFor(req) # "none" =>
        /\ response.outcome = "rejected"
        /\ response.read_count = 0

CwdReadRowsStayScoped ==
    req.kind = "read" /\ response.outcome = "ok" /\ req.session = "none" =>
        response.result_cwds \subseteq {req.cwd}

InvalidPromotionsDoNotWrite ==
    req.kind = "promote" /\ PromotionReasonFor(req) \in
        {"invalid_scope", "invalid_project", "invalid_target",
         "file_busy", "inactive_source", "already_promoted_different_target"} =>
            /\ response.outcome = "rejected"
            /\ response.db_writes = 0
            /\ response.file_writes = 0

RepeatedPromotionIsIdempotent ==
    req.kind = "promote" /\ response.outcome = "ok" /\ req.existing =>
        /\ response.durable_count = 1
        /\ response.inserted_durable = FALSE

NewPromotionCreatesOneDurable ==
    req.kind = "promote" /\ response.outcome = "ok" /\ ~req.existing =>
        /\ response.durable_count = 1
        /\ response.inserted_durable = TRUE

FileLockIsNonBlocking ==
    req.kind = "promote" /\ FileLockAttemptedFor(req) /\ req.file_lock = "busy" =>
        /\ response.reason = "file_busy"
        /\ response.outcome = "rejected"
        /\ response.db_writes = 0

NoHeldLocksAtReturn ==
    response.lock_held_end = FALSE

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidReadsReject /\
        CwdReadRowsStayScoped /\
        InvalidPromotionsDoNotWrite /\
        RepeatedPromotionIsIdempotent /\
        NewPromotionCreatesOneDurable /\
        FileLockIsNonBlocking /\
        NoHeldLocksAtReturn)

================================================================================
