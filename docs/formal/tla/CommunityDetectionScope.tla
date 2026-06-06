---------------------------- MODULE CommunityDetectionScope ----------------------------
(***************************************************************************)
(* `community_detection` request/scoping model.                            *)
(*                                                                         *)
(* The tool resolves one project id, validates graph_type, clamps a finite  *)
(* Louvain resolution, rejects stale edge/file project mismatches, and      *)
(* emits a numeric community/modularity envelope without writes or locks.   *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

ProjectIds == {0, 1, 2}
ProjectModes == {"unique", "blank", "missing", "duplicate"}
GraphModes == {"import", "co_change", "combined", "blank", "bad"}
ResolutionModes == {"default", "low", "high", "nonfinite"}
Outcomes == {"ok", "rejected"}
Reasons == {"none", "invalid_project", "invalid_graph_type", "nonfinite_resolution"}
GraphTypes == {"none", "import", "co_change", "combined"}
Files == {"core/a.rs", "core/b.rs", "leak.rs"}

Edges ==
    { [edge_project |-> 1, source_project |-> 1, target_project |-> 1,
       edge_type |-> "import", source |-> "core/a.rs", target |-> "core/b.rs"],
      [edge_project |-> 1, source_project |-> 2, target_project |-> 2,
       edge_type |-> "import", source |-> "leak.rs", target |-> "leak.rs"],
      [edge_project |-> 1, source_project |-> 1, target_project |-> 2,
       edge_type |-> "import", source |-> "core/a.rs", target |-> "leak.rs"],
      [edge_project |-> 1, source_project |-> 1, target_project |-> 1,
       edge_type |-> "co_change", source |-> "core/b.rs", target |-> "core/a.rs"] }

Requests ==
    { [id |-> 1, project_mode |-> "unique", graph_mode |-> "blank",
       resolution_mode |-> "default"],
      [id |-> 2, project_mode |-> "unique", graph_mode |-> "combined",
       resolution_mode |-> "high"],
      [id |-> 3, project_mode |-> "blank", graph_mode |-> "import",
       resolution_mode |-> "default"],
      [id |-> 4, project_mode |-> "duplicate", graph_mode |-> "import",
       resolution_mode |-> "default"],
      [id |-> 5, project_mode |-> "unique", graph_mode |-> "bad",
       resolution_mode |-> "default"],
      [id |-> 6, project_mode |-> "unique", graph_mode |-> "import",
       resolution_mode |-> "nonfinite"],
      [id |-> 7, project_mode |-> "unique", graph_mode |-> "co_change",
       resolution_mode |-> "low"] }

RequestIds == {r.id : r \in Requests}

ProjectFor(r) ==
    IF r.project_mode = "unique" THEN 1 ELSE 0

GraphTypeFor(r) ==
    CASE r.graph_mode = "blank" -> "import"
      [] r.graph_mode \in {"import", "co_change", "combined"} -> r.graph_mode
      [] OTHER -> "none"

ResolutionFor(r) ==
    CASE r.resolution_mode = "low" -> 1
      [] r.resolution_mode = "high" -> 10
      [] r.resolution_mode = "default" -> 1
      [] OTHER -> 0

ReasonFor(r) ==
    CASE r.project_mode # "unique" -> "invalid_project"
      [] r.graph_mode = "bad" -> "invalid_graph_type"
      [] r.resolution_mode = "nonfinite" -> "nonfinite_resolution"
      [] OTHER -> "none"

EdgeAccepted(r, e) ==
    /\ e.edge_project = ProjectFor(r)
    /\ e.source_project = ProjectFor(r)
    /\ e.target_project = ProjectFor(r)
    /\ (GraphTypeFor(r) = "combined" \/ e.edge_type = GraphTypeFor(r))

ScopedEdgesFor(r) ==
    IF ReasonFor(r) # "none" THEN {}
    ELSE {e \in Edges : EdgeAccepted(r, e)}

FilesFor(r) ==
    {e.source : e \in ScopedEdgesFor(r)} \cup {e.target : e \in ScopedEdgesFor(r)}

ResponseFor(r) ==
    LET reason == ReasonFor(r) IN
    LET ok == reason = "none" IN
        [ request_id |-> r.id,
          outcome |-> IF ok THEN "ok" ELSE "rejected",
          reason |-> reason,
          project_id |-> IF ok THEN ProjectFor(r) ELSE 0,
          graph_type |-> IF ok THEN GraphTypeFor(r) ELSE "none",
          resolution |-> IF ok THEN ResolutionFor(r) ELSE 0,
          result_files |-> FilesFor(r),
          edge_count |-> Cardinality(ScopedEdgesFor(r)),
          modularity_numeric |-> ok,
          community_count |-> IF ok THEN Cardinality(FilesFor(r)) ELSE 0,
          effect_project_id |-> IF ok THEN ProjectFor(r) ELSE 0,
          writes |-> 0,
          lock_held |-> FALSE ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      reason: Reasons,
      project_id: ProjectIds,
      graph_type: GraphTypes,
      resolution: 0..10,
      result_files: SUBSET Files,
      edge_count: 0..4,
      modularity_numeric: BOOLEAN,
      community_count: 0..3,
      effect_project_id: ProjectIds,
      writes: 0..0,
      lock_held: BOOLEAN ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK == response \in ResponseRecord

InvalidRequestsReject ==
    ReasonFor(req) # "none" =>
        /\ response.outcome = "rejected"
        /\ response.edge_count = 0

GraphTypeClosed ==
    response.outcome = "ok" => response.graph_type \in {"import", "co_change", "combined"}

ResolutionBounded ==
    response.outcome = "ok" => response.resolution \in 1..10

StaleEdgesExcluded ==
    response.outcome = "ok" /\ response.project_id = 1 =>
        "leak.rs" \notin response.result_files

EffectEnrichmentUsesResolvedProject ==
    response.outcome = "ok" => response.effect_project_id = response.project_id

NumericCommunityEnvelope ==
    response.outcome = "ok" =>
        /\ response.modularity_numeric
        /\ response.community_count >= 0

ReadOnlyNoHeldLock ==
    /\ response.writes = 0
    /\ response.lock_held = FALSE

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidRequestsReject /\
        GraphTypeClosed /\
        ResolutionBounded /\
        StaleEdgesExcluded /\
        EffectEnrichmentUsesResolvedProject /\
        NumericCommunityEnvelope /\
        ReadOnlyNoHeldLock)

================================================================================
