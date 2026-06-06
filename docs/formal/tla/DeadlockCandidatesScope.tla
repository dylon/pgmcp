----------------------------- MODULE DeadlockCandidatesScope -----------------------------
(***************************************************************************)
(* `deadlock_candidates` scoping/read-only model.                          *)
(*                                                                         *)
(* The legacy tool scans indexed file content in one resolved project,      *)
(* constructs adjacent lock-order edges, reports SCC cycles, and enriches   *)
(* with mutex-typed symbols plus effect counts. All channels must use the   *)
(* same project id and remain read-only.                                    *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

ProjectModes == {"valid", "missing"}
Outcomes == {"ok", "rejected"}
Locks == {"alpha", "beta", "gamma", "delta", "none"}
Effects == {"unsafe", "none"}

Requests ==
    { [id |-> 1, project |-> "valid"],
      [id |-> 2, project |-> "missing"] }

RequestIds == {r.id : r \in Requests}

ValidProject(r) == r.project = "valid"

ResponseFor(r) ==
    [ request_id |-> r.id,
      outcome |-> IF ValidProject(r) THEN "ok" ELSE "rejected",
      scanned_target_files |-> IF ValidProject(r) THEN TRUE ELSE FALSE,
      scanned_other_files |-> FALSE,
      edge_ab |-> ValidProject(r),
      edge_ba |-> ValidProject(r),
      edge_gd |-> FALSE,
      edge_dg |-> FALSE,
      cycle_alpha_beta |-> ValidProject(r),
      cycle_gamma_delta |-> FALSE,
      mutex_symbols_project_scoped |-> ValidProject(r),
      effect_unsafe_count |-> 0,
      wrote_db |-> FALSE,
      lock_held |-> FALSE ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      scanned_target_files: BOOLEAN,
      scanned_other_files: BOOLEAN,
      edge_ab: BOOLEAN,
      edge_ba: BOOLEAN,
      edge_gd: BOOLEAN,
      edge_dg: BOOLEAN,
      cycle_alpha_beta: BOOLEAN,
      cycle_gamma_delta: BOOLEAN,
      mutex_symbols_project_scoped: BOOLEAN,
      effect_unsafe_count: 0..1,
      wrote_db: BOOLEAN,
      lock_held: BOOLEAN ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    response \in ResponseRecord

InvalidProjectNoScan ==
    ~ValidProject(req) =>
        /\ response.outcome = "rejected"
        /\ ~response.scanned_target_files
        /\ ~response.scanned_other_files
        /\ ~response.edge_ab
        /\ ~response.cycle_alpha_beta

FileScanProjectScoped ==
    /\ response.scanned_target_files = ValidProject(req)
    /\ ~response.scanned_other_files

EdgesProjectScoped ==
    /\ response.edge_ab = ValidProject(req)
    /\ response.edge_ba = ValidProject(req)
    /\ ~response.edge_gd
    /\ ~response.edge_dg

CyclesFromScopedEdgesOnly ==
    /\ response.cycle_alpha_beta = (response.edge_ab /\ response.edge_ba)
    /\ ~response.cycle_gamma_delta

EffectBreakdownProjectScoped ==
    response.effect_unsafe_count = 0

MutexTypedSymbolsProjectScoped ==
    response.mutex_symbols_project_scoped = ValidProject(req)

ReadOnlyNoLock ==
    /\ ~response.wrote_db
    /\ ~response.lock_held

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidProjectNoScan /\
        FileScanProjectScoped /\
        EdgesProjectScoped /\
        CyclesFromScopedEdgesOnly /\
        EffectBreakdownProjectScoped /\
        MutexTypedSymbolsProjectScoped /\
        ReadOnlyNoLock)

================================================================================
