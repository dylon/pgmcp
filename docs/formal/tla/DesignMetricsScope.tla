------------------------------- MODULE DesignMetricsScope -------------------------------
(***************************************************************************)
(* `design_metrics` request boundary.                                      *)
(*                                                                         *)
(* The tool resolves one project display name, applies an optional         *)
(* project/module/file scope, computes per-file metrics, and enriches from *)
(* AST/effect tables. Correctness requires the row query and enrichment    *)
(* queries to use the same resolved project id.                            *)
(***************************************************************************)

EXTENDS Integers, Sequences, FiniteSets

Projects ==
    { [id |-> 1, name |-> "unique"],
      [id |-> 2, name |-> "duplicate"],
      [id |-> 3, name |-> "duplicate"] }

Files ==
    { [id |-> 10, project_id |-> 1, relative_path |-> "core/a.rs"],
      [id |-> 20, project_id |-> 1, relative_path |-> "core/b.rs"],
      [id |-> 30, project_id |-> 1, relative_path |-> "api/api.rs"],
      [id |-> 40, project_id |-> 1, relative_path |-> "core/%literal.rs"],
      [id |-> 50, project_id |-> 2, relative_path |-> "core/a.rs"],
      [id |-> 60, project_id |-> 3, relative_path |-> "core/a.rs"] }

NoReq == [id |-> 0, project |-> "", scope |-> "project", path |-> "", limit |-> 30]

Requests ==
    { [id |-> 1, project |-> "unique", scope |-> "project", path |-> "", limit |-> -5],
      [id |-> 2, project |-> "unique", scope |-> "project", path |-> "", limit |-> 500],
      [id |-> 3, project |-> "unique", scope |-> "module", path |-> "core/", limit |-> 30],
      [id |-> 4, project |-> "unique", scope |-> "directory", path |-> "core/", limit |-> 30],
      [id |-> 5, project |-> "unique", scope |-> "module", path |-> "core/%", limit |-> 30],
      [id |-> 6, project |-> "unique", scope |-> "file", path |-> "core/a.rs", limit |-> 30],
      [id |-> 7, project |-> "duplicate", scope |-> "project", path |-> "", limit |-> 30],
      [id |-> 8, project |-> "missing", scope |-> "project", path |-> "", limit |-> 30],
      [id |-> 9, project |-> "unique", scope |-> "weird", path |-> "", limit |-> 30] }

RequestIds == {r.id : r \in Requests}
ProjectIds == {p.id : p \in Projects}
Outcomes == {"ok", "rejected"}
EffectiveScopes == {"project", "module", "file", "invalid"}

ClampLimit(limit) ==
    IF limit < 1 THEN 1 ELSE IF limit > 100 THEN 100 ELSE limit

Matches(project_name) == {p \in Projects : p.name = project_name}

ResolvedProjectId(r) ==
    IF Cardinality(Matches(r.project)) = 1
    THEN (CHOOSE p \in Matches(r.project) : TRUE).id
    ELSE 0

ValidScope(scope) ==
    scope \in {"project", "module", "directory", "file"}

EffectiveScope(scope) ==
    IF scope = "directory" THEN "module"
    ELSE IF scope \in {"project", "module", "file"} THEN scope
    ELSE "invalid"

PrefixMatch(path, prefix) ==
    \/ prefix = ""
    \/ /\ prefix = "core/"
       /\ path \in {"core/a.rs", "core/b.rs", "core/%literal.rs"}
    \/ /\ prefix = "core/%"
       /\ path = "core/%literal.rs"
    \/ /\ prefix = "api/"
       /\ path = "api/api.rs"

VisibleFiles(r) ==
    LET pid == ResolvedProjectId(r) IN
    LET scope == EffectiveScope(r.scope) IN
    IF scope = "project" \/ r.path = "" THEN
        {f \in Files : f.project_id = pid}
    ELSE IF scope = "module" THEN
        {f \in Files : f.project_id = pid /\ PrefixMatch(f.relative_path, r.path)}
    ELSE IF scope = "file" THEN
        {f \in Files : f.project_id = pid /\ f.relative_path = r.path}
    ELSE {}

BoundedRows(r) ==
    LET visible == VisibleFiles(r) IN
    LET cap == ClampLimit(r.limit) IN
    IF Cardinality(visible) <= cap THEN visible
    ELSE {CHOOSE f \in visible : TRUE}

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      resolved_project_id: ProjectIds \cup {0},
      metric_project_id: ProjectIds \cup {0},
      effect_project_id: ProjectIds \cup {0},
      effective_scope: EffectiveScopes,
      effective_limit: 1..100,
      rows: SUBSET Files ]

Init ==
    /\ req \in Requests
    /\ LET cap == ClampLimit(req.limit) IN
       IF Cardinality(Matches(req.project)) # 1 \/ ~ValidScope(req.scope) THEN
        response =
            [ request_id |-> req.id,
              outcome |-> "rejected",
              resolved_project_id |-> 0,
              metric_project_id |-> 0,
              effect_project_id |-> 0,
              effective_scope |-> EffectiveScope(req.scope),
              effective_limit |-> ClampLimit(req.limit),
              rows |-> {} ]
       ELSE
       LET pid == ResolvedProjectId(req) IN
       LET rows == BoundedRows(req) IN
       /\ Cardinality(rows) <= cap
       /\ response =
           [ request_id |-> req.id,
             outcome |-> "ok",
             resolved_project_id |-> pid,
             metric_project_id |-> pid,
             effect_project_id |-> pid,
             effective_scope |-> EffectiveScope(req.scope),
             effective_limit |-> cap,
             rows |-> rows ]

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests
    /\ response \in ResponseRecord

InvalidRequestsRejected ==
    (Cardinality(Matches(req.project)) # 1 \/ ~ValidScope(req.scope)) =>
        /\ response.outcome = "rejected"
        /\ response.rows = {}

RowsProjectScoped ==
    \A row \in response.rows :
        row.project_id = response.resolved_project_id

ScopeFilterSound ==
    \A row \in response.rows :
        \/ response.effective_scope = "project"
        \/ req.path = ""
        \/ /\ response.effective_scope = "module"
           /\ PrefixMatch(row.relative_path, req.path)
        \/ /\ response.effective_scope = "file"
           /\ row.relative_path = req.path

EffectiveLimitClamped ==
    response.effective_limit = ClampLimit(req.limit)

OutputWithinLimit ==
    Cardinality(response.rows) <= response.effective_limit

EnrichmentUsesResolvedProject ==
    response.outcome = "ok" =>
        /\ response.metric_project_id = response.resolved_project_id
        /\ response.effect_project_id = response.resolved_project_id

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidRequestsRejected /\
        RowsProjectScoped /\
        ScopeFilterSound /\
        EffectiveLimitClamped /\
        OutputWithinLimit /\
        EnrichmentUsesResolvedProject)

=============================================================================
