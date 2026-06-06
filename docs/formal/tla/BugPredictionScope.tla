------------------------------- MODULE BugPredictionScope -------------------------------
(***************************************************************************)
(* `bug_prediction` request boundary.                                      *)
(*                                                                         *)
(* The tool resolves one project, loads file metrics for that project,      *)
(* scores/ranks them, and enriches with bug-prone effect symbols. The       *)
(* local safety obligations are duplicate-name rejection, bounded output,   *)
(* project-scoped metric rows, and effect enrichment using the same         *)
(* resolved project id.                                                     *)
(***************************************************************************)

EXTENDS Integers, Sequences, FiniteSets

Projects ==
    { [id |-> 1, name |-> "unique"],
      [id |-> 2, name |-> "duplicate"],
      [id |-> 3, name |-> "duplicate"] }

Files ==
    { [id |-> 10, project_id |-> 1, path |-> "unique/a.rs", has_bug_label |-> TRUE],
      [id |-> 20, project_id |-> 1, path |-> "unique/b.rs", has_bug_label |-> FALSE],
      [id |-> 30, project_id |-> 1, path |-> "unique/c.rs", has_bug_label |-> FALSE],
      [id |-> 40, project_id |-> 2, path |-> "dup-left/a.rs", has_bug_label |-> TRUE],
      [id |-> 50, project_id |-> 3, path |-> "dup-right/a.rs", has_bug_label |-> TRUE] }

EffectSymbols ==
    { [project_id |-> 1, file_id |-> 10, effect |-> "unsafe"],
      [project_id |-> 1, file_id |-> 20, effect |-> "may_panic"],
      [project_id |-> 2, file_id |-> 40, effect |-> "unsafe"],
      [project_id |-> 3, file_id |-> 50, effect |-> "unsafe"] }

Requests ==
    { [id |-> 1, project |-> "unique", limit |-> -10],
      [id |-> 2, project |-> "unique", limit |-> 500],
      [id |-> 3, project |-> "unique", limit |-> 2],
      [id |-> 4, project |-> "duplicate", limit |-> 20],
      [id |-> 5, project |-> "missing", limit |-> 20] }

RequestIds == {r.id : r \in Requests}
ProjectIds == {p.id : p \in Projects}
Outcomes == {"ok", "rejected"}
ScoreKinds == {"trained_logreg", "heuristic"}

ClampLimit(limit) ==
    IF limit < 1 THEN 1 ELSE IF limit > 100 THEN 100 ELSE limit

Matches(project_name) == {p \in Projects : p.name = project_name}

ResolvedProjectId(r) ==
    IF Cardinality(Matches(r.project)) = 1
    THEN (CHOOSE p \in Matches(r.project) : TRUE).id
    ELSE 0

VisibleFiles(r) ==
    {f \in Files : f.project_id = ResolvedProjectId(r)}

VisibleEffects(r) ==
    {s \in EffectSymbols : s.project_id = ResolvedProjectId(r)}

BoundedRows(r) ==
    LET visible == VisibleFiles(r) IN
    LET cap == ClampLimit(r.limit) IN
    IF Cardinality(visible) <= cap THEN visible
    ELSE {CHOOSE f \in visible : TRUE}

HasBothTrainingClasses(rows) ==
    /\ \E row \in rows : row.has_bug_label
    /\ \E row \in rows : ~row.has_bug_label

ScoreKindFor(rows) ==
    IF HasBothTrainingClasses(rows) THEN "trained_logreg" ELSE "heuristic"

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      resolved_project_id: ProjectIds \cup {0},
      effect_project_id: ProjectIds \cup {0},
      effective_limit: 1..100,
      score_kind: ScoreKinds,
      rows: SUBSET Files,
      effect_symbols: SUBSET EffectSymbols ]

Init ==
    /\ req \in Requests
    /\ LET cap == ClampLimit(req.limit) IN
       IF Cardinality(Matches(req.project)) # 1 THEN
        response =
            [ request_id |-> req.id,
              outcome |-> "rejected",
              resolved_project_id |-> 0,
              effect_project_id |-> 0,
              effective_limit |-> cap,
              score_kind |-> "heuristic",
              rows |-> {},
              effect_symbols |-> {} ]
       ELSE
       LET pid == ResolvedProjectId(req) IN
       LET rows == BoundedRows(req) IN
       /\ Cardinality(rows) <= cap
       /\ response =
           [ request_id |-> req.id,
             outcome |-> "ok",
             resolved_project_id |-> pid,
             effect_project_id |-> pid,
             effective_limit |-> cap,
             score_kind |-> ScoreKindFor(VisibleFiles(req)),
             rows |-> rows,
             effect_symbols |-> VisibleEffects(req) ]

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests
    /\ response \in ResponseRecord

NonUniqueProjectRejected ==
    Cardinality(Matches(req.project)) # 1 =>
        /\ response.outcome = "rejected"
        /\ response.rows = {}
        /\ response.effect_symbols = {}
        /\ response.resolved_project_id = 0

RowsProjectScoped ==
    \A row \in response.rows :
        row.project_id = response.resolved_project_id

EffectSymbolsProjectScoped ==
    \A symbol \in response.effect_symbols :
        symbol.project_id = response.resolved_project_id

EffectiveLimitClamped ==
    response.effective_limit = ClampLimit(req.limit)

OutputWithinLimit ==
    Cardinality(response.rows) <= response.effective_limit

ScoreKindMatchesTrainingData ==
    response.outcome = "ok" =>
        response.score_kind = ScoreKindFor(VisibleFiles(req))

EnrichmentUsesResolvedProject ==
    response.outcome = "ok" =>
        response.effect_project_id = response.resolved_project_id

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        NonUniqueProjectRejected /\
        RowsProjectScoped /\
        EffectSymbolsProjectScoped /\
        EffectiveLimitClamped /\
        OutputWithinLimit /\
        ScoreKindMatchesTrainingData /\
        EnrichmentUsesResolvedProject)

=============================================================================
