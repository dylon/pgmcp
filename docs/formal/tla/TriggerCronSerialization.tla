------------------------------- MODULE TriggerCronSerialization -------------------------------
(***************************************************************************)
(* `trigger_cron` dispatch and heavy-cron serialization.                   *)
(*                                                                         *)
(* The manual MCP trigger shares the same non-blocking heavy-cron lock as   *)
(* scheduled maintenance jobs. Invalid jobs fail before lock acquisition.   *)
(* Valid jobs either acquire the lock, run exactly one body, and release it *)
(* or observe an already-held lock and return a structured busy response.   *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

ValidJobs ==
    {"symbol-extraction", "call-graph", "function-metrics", "graph-analysis",
     "a2a-reflect", "msm-calibrate", "fuzzy-sync"}

ProjectScopedJobs == {"symbol-extraction", "call-graph", "function-metrics"}
RawJobs == ValidJobs \cup {" graph-analysis ", "unknown", "   "}
RawProjects == {"", "   ", " pgmcp "}
Projects == {"none", "pgmcp"}
InitialLocks == {"free", "held"}
Statuses == {"completed", "busy", "rejected"}
Reasons == {"none", "invalid_job", "blank_job", "lock_busy"}

Requests ==
    { [id |-> 1, job |-> " graph-analysis ", project |-> "   "],
      [id |-> 2, job |-> "symbol-extraction", project |-> " pgmcp "],
      [id |-> 3, job |-> "unknown", project |-> ""],
      [id |-> 4, job |-> "   ", project |-> ""],
      [id |-> 5, job |-> "fuzzy-sync", project |-> " pgmcp "] }

RequestIds == {r.id : r \in Requests}

Trim(s) ==
    CASE s = " graph-analysis " -> "graph-analysis"
      [] s = " pgmcp " -> "pgmcp"
      [] s = "   " -> ""
      [] OTHER -> s

ProjectFor(raw) ==
    LET p == Trim(raw) IN IF p = "" THEN "none" ELSE p

ReasonFor(r, initial_lock) ==
    LET job == Trim(r.job) IN
        CASE job = "" -> "blank_job"
          [] ~(job \in ValidJobs) -> "invalid_job"
          [] initial_lock = "held" -> "lock_busy"
          [] OTHER -> "none"

ResponseFor(r, initial_lock) ==
    LET job == Trim(r.job) IN
    LET project == ProjectFor(r.project) IN
    LET reason == ReasonFor(r, initial_lock) IN
    LET completed == reason = "none" IN
    LET busy == reason = "lock_busy" IN
        [ request_id |-> r.id,
          initial_lock |-> initial_lock,
          normalized_job |-> job,
          project |-> project,
          status |->
              IF completed THEN "completed"
              ELSE IF busy THEN "busy"
              ELSE "rejected",
          reason |-> reason,
          lock_acquired |-> completed,
          body_started |-> completed,
          queued |-> FALSE,
          lock_after |-> initial_lock,
          heavy_flag_during |-> completed,
          heavy_flag_after |-> FALSE,
          project_forwarded |-> completed /\ job \in ProjectScopedJobs /\ project # "none" ]

VARIABLES req, initial_lock, response

vars == <<req, initial_lock, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      initial_lock: InitialLocks,
      normalized_job: ValidJobs \cup {"", "unknown"},
      project: Projects,
      status: Statuses,
      reason: Reasons,
      lock_acquired: BOOLEAN,
      body_started: BOOLEAN,
      queued: BOOLEAN,
      lock_after: InitialLocks,
      heavy_flag_during: BOOLEAN,
      heavy_flag_after: BOOLEAN,
      project_forwarded: BOOLEAN ]

Init ==
    /\ req \in Requests
    /\ initial_lock \in InitialLocks
    /\ response = ResponseFor(req, initial_lock)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests
    /\ initial_lock \in InitialLocks
    /\ response \in ResponseRecord

InvalidJobsRejectedBeforeLock ==
    Trim(req.job) = "" \/ ~(Trim(req.job) \in ValidJobs) =>
        /\ response.status = "rejected"
        /\ response.lock_acquired = FALSE
        /\ response.body_started = FALSE
        /\ response.lock_after = initial_lock

BusyNeverRunsBody ==
    Trim(req.job) \in ValidJobs /\ initial_lock = "held" =>
        /\ response.status = "busy"
        /\ response.reason = "lock_busy"
        /\ response.lock_acquired = FALSE
        /\ response.body_started = FALSE
        /\ response.queued = FALSE
        /\ response.lock_after = "held"

CompletedOnlyFromFreeLock ==
    response.status = "completed" =>
        /\ initial_lock = "free"
        /\ response.lock_acquired = TRUE
        /\ response.body_started = TRUE
        /\ response.queued = FALSE

LockReleasedAfterCompletion ==
    response.status = "completed" =>
        /\ response.lock_after = "free"
        /\ response.heavy_flag_during = TRUE
        /\ response.heavy_flag_after = FALSE

NoQueueing ==
    response.queued = FALSE

NormalizedAcceptedJob ==
    response.status \in {"completed", "busy"} =>
        response.normalized_job \in ValidJobs

ProjectNormalized ==
    response.project = ProjectFor(req.project)

ProjectForwardingOnlyForScopedJobs ==
    response.project_forwarded =>
        /\ response.status = "completed"
        /\ response.normalized_job \in ProjectScopedJobs
        /\ response.project # "none"

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidJobsRejectedBeforeLock /\
        BusyNeverRunsBody /\
        CompletedOnlyFromFreeLock /\
        LockReleasedAfterCompletion /\
        NoQueueing /\
        NormalizedAcceptedJob /\
        ProjectNormalized /\
        ProjectForwardingOnlyForScopedJobs)

=============================================================================
