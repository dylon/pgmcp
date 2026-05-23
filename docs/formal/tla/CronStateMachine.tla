---- MODULE CronStateMachine ----
(*
 * pgmcp cron task lifecycle: pending → running → completed.
 *
 * Models the heavy-cron skip-gate logic in src/cron/scheduler.rs:
 * each cron tick acquires the heavy_cron_lock; if it's held, the
 * tick records a Skip outcome and returns. The lock guarantees at
 * most one heavy cron runs at a time.
 *
 * Invariants:
 *   DisjointSets — every task is in exactly one of pending /
 *     running / completed at any state.
 *   NoDoubleProcessing — a task that is completed never re-enters
 *     pending or running.
 *   AtMostOneRunning — the heavy_cron_lock ensures the running set
 *     has cardinality ≤ 1.
 *
 * Refinement:
 *   RecoverInProgressAsFailed — on crash recovery, any task in
 *     `running` is moved to `pending` (the recovery cron at daemon
 *     start re-queues uncompleted work). Captures the
 *     `feedback_long_running_jobs_must_handle_fk_drift.md`
 *     post-mortem: jobs interrupted mid-pass must be retryable.
 *
 * Plan reference:
 *   ~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md
 *   Phase 12.
 *)
EXTENDS Naturals, FiniteSets

CONSTANT Tasks

VARIABLES pending, running, completed, lock_held

vars == <<pending, running, completed, lock_held>>

TypeOK ==
    /\ pending \subseteq Tasks
    /\ running \subseteq Tasks
    /\ completed \subseteq Tasks
    /\ lock_held \in BOOLEAN

Init ==
    /\ pending = Tasks
    /\ running = {}
    /\ completed = {}
    /\ lock_held = FALSE

(* A pending task acquires the lock and starts running.            *)
StartTask(t) ==
    /\ t \in pending
    /\ ~lock_held
    /\ pending' = pending \ {t}
    /\ running' = running \cup {t}
    /\ lock_held' = TRUE
    /\ UNCHANGED completed

(* A running task finishes successfully and releases the lock.     *)
CompleteTask(t) ==
    /\ t \in running
    /\ running' = running \ {t}
    /\ completed' = completed \cup {t}
    /\ lock_held' = FALSE
    /\ UNCHANGED pending

(* A running task is interrupted (daemon crash / SIGTERM); it's
 * moved back to pending and the lock is released so a later run
 * can pick it up.                                                 *)
RecoverInProgressAsFailed(t) ==
    /\ t \in running
    /\ running' = running \ {t}
    /\ pending' = pending \cup {t}
    /\ lock_held' = FALSE
    /\ UNCHANGED completed

Next ==
    \/ \E t \in pending : StartTask(t)
    \/ \E t \in running : CompleteTask(t)
    \/ \E t \in running : RecoverInProgressAsFailed(t)

Spec == Init /\ [][Next]_vars

(* === Safety invariants === *)

DisjointSets ==
    /\ pending \cap running   = {}
    /\ pending \cap completed = {}
    /\ running \cap completed = {}

AllAccountedFor ==
    pending \cup running \cup completed = Tasks

NoDoubleProcessing ==
    \A t \in completed : t \notin pending /\ t \notin running

AtMostOneRunning ==
    Cardinality(running) <= 1

LockMatchesRunning ==
    (lock_held = TRUE) <=> (Cardinality(running) = 1)

Invariants ==
    /\ TypeOK
    /\ DisjointSets
    /\ AllAccountedFor
    /\ NoDoubleProcessing
    /\ AtMostOneRunning
    /\ LockMatchesRunning

====
