----------------------------- MODULE ChangeImpactScope -----------------------------
(***************************************************************************)
(* `change_impact_analysis` request/scoping model.                         *)
(*                                                                         *)
(* The tool resolves one unique project and one relative file, then merges  *)
(* reverse import BFS, co-change, optional semantic similarity, resolved    *)
(* caller reachability, effect counts, and cross-project dependents.        *)
(* File-level channels must stay inside the resolved project; cross-project *)
(* dependents are the only intentionally cross-project output.              *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

ProjectModes == {"valid_unique", "missing", "duplicate"}
FileModes == {"valid", "blank", "missing"}
Outcomes == {"ok", "rejected"}

Requests ==
    { [id |-> 1, project |-> "valid_unique", file |-> "valid",
       raw_depth |-> 99, semantic |-> FALSE],
      [id |-> 2, project |-> "duplicate", file |-> "valid",
       raw_depth |-> 3, semantic |-> FALSE],
      [id |-> 3, project |-> "missing", file |-> "valid",
       raw_depth |-> 3, semantic |-> FALSE],
      [id |-> 4, project |-> "valid_unique", file |-> "blank",
       raw_depth |-> 3, semantic |-> FALSE],
      [id |-> 5, project |-> "valid_unique", file |-> "missing",
       raw_depth |-> 3, semantic |-> FALSE],
      [id |-> 6, project |-> "valid_unique", file |-> "valid",
       raw_depth |-> 0, semantic |-> TRUE] }

RequestIds == {r.id : r \in Requests}

ValidProject(r) == r.project = "valid_unique"
ValidFile(r) == r.file = "valid"
Accepted(r) == ValidProject(r) /\ ValidFile(r)

DepthFor(r) ==
    IF r.raw_depth < 1 THEN 1
    ELSE IF r.raw_depth > 12 THEN 12
    ELSE r.raw_depth

ResponseFor(r) ==
    [ request_id |-> r.id,
      outcome |-> IF Accepted(r) THEN "ok" ELSE "rejected",
      depth |-> DepthFor(r),
      import_same_project |-> Accepted(r),
      import_cross_project |-> FALSE,
      cochange_same_project |-> Accepted(r),
      cochange_cross_project |-> FALSE,
      semantic_same_project |-> Accepted(r) /\ r.semantic,
      semantic_cross_project |-> FALSE,
      resolved_caller_same_project |-> Accepted(r),
      resolved_caller_cross_project |-> FALSE,
      effect_unsafe_count |-> 0,
      cross_project_dependents_allowed |-> Accepted(r),
      wrote_db |-> FALSE,
      lock_held |-> FALSE ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      depth: 1..12,
      import_same_project: BOOLEAN,
      import_cross_project: BOOLEAN,
      cochange_same_project: BOOLEAN,
      cochange_cross_project: BOOLEAN,
      semantic_same_project: BOOLEAN,
      semantic_cross_project: BOOLEAN,
      resolved_caller_same_project: BOOLEAN,
      resolved_caller_cross_project: BOOLEAN,
      effect_unsafe_count: 0..1,
      cross_project_dependents_allowed: BOOLEAN,
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

InvalidProjectOrFileRejects ==
    ~Accepted(req) =>
        /\ response.outcome = "rejected"
        /\ ~response.import_same_project
        /\ ~response.resolved_caller_same_project
        /\ ~response.cross_project_dependents_allowed

DepthClamped ==
    response.depth \in 1..12

ImportRowsProjectScoped ==
    /\ response.import_same_project = Accepted(req)
    /\ ~response.import_cross_project

CochangeRowsProjectScoped ==
    /\ response.cochange_same_project = Accepted(req)
    /\ ~response.cochange_cross_project

SemanticRowsProjectScopedAndOptional ==
    /\ response.semantic_same_project = (Accepted(req) /\ req.semantic)
    /\ ~response.semantic_cross_project

ResolvedCallerRowsProjectScoped ==
    /\ response.resolved_caller_same_project = Accepted(req)
    /\ ~response.resolved_caller_cross_project

EffectBreakdownProjectScoped ==
    response.effect_unsafe_count = 0

OnlyProjectDependentsMayCrossProject ==
    response.cross_project_dependents_allowed = Accepted(req)

ReadOnlyNoLock ==
    /\ ~response.wrote_db
    /\ ~response.lock_held

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidProjectOrFileRejects /\
        DepthClamped /\
        ImportRowsProjectScoped /\
        CochangeRowsProjectScoped /\
        SemanticRowsProjectScopedAndOptional /\
        ResolvedCallerRowsProjectScoped /\
        EffectBreakdownProjectScoped /\
        OnlyProjectDependentsMayCrossProject /\
        ReadOnlyNoLock)

================================================================================
