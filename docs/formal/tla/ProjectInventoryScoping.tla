-------------------------- MODULE ProjectInventoryScoping --------------------------
(***************************************************************************)
(* Project-scoped inventory resolution for list_projects/mandate_context/   *)
(* orient/project_tree/file_info and cwd-based project lookup.              *)
(*                                                                         *)
(* The model covers the fail-closed rules added after verifying the most    *)
(* commonly used search/inventory tools:                                    *)
(*                                                                         *)
(*   - duplicate display names are ambiguous, not merged;                  *)
(*   - list_projects preserves distinct project identities, even when      *)
(*     display names are duplicated;                                       *)
(*   - name-scoped tools only return rows from the selected project;        *)
(*   - file_info is an exact absolute-path lookup;                          *)
(*   - find_project_by_cwd matches only exact paths or path-component       *)
(*     children, so /ws/app does not match /ws/application.                 *)
(*                                                                         *)
(* This is intentionally a small finite snapshot model. The paired Rust     *)
(* query smoke tests execute the production SQL against PostgreSQL.          *)
(***************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets, TLC

Projects == {1, 2, 3, 4}

ProjectName ==
    ( 1 :> "boundary-app"
   @@ 2 :> "duplicate-display-name"
   @@ 3 :> "duplicate-display-name"
   @@ 4 :> "proj-auth" )

ProjectPath ==
    ( 1 :> <<"ws", "boundary", "app">>
   @@ 2 :> <<"ws", "ambiguous-a", "shared">>
   @@ 3 :> <<"ws", "ambiguous-b", "shared">>
   @@ 4 :> <<"ws", "auth">> )

Files == {101, 102, 103, 104}

FileProject == (101 :> 1 @@ 102 :> 2 @@ 103 :> 3 @@ 104 :> 4)

FileAbs ==
    ( 101 :> "/ws/boundary/app/src/lib.rs"
   @@ 102 :> "/ws/ambiguous-a/shared/a.rs"
   @@ 103 :> "/ws/ambiguous-b/shared/b.rs"
   @@ 104 :> "/ws/auth/auth/file_0.rs" )

NameScopedTools == {"mandate_context", "orient", "project_tree"}

NoReq == [tool |-> "none", project |-> "", path |-> "", cwd |-> <<>>]

ApplicationSiblingCwd == <<"ws", "boundary", "application", "src">>

Requests ==
    { [tool |-> "list_projects", project |-> "", path |-> "", cwd |-> <<>>],
      [tool |-> "orient", project |-> "proj-auth", path |-> "", cwd |-> <<>>],
      [tool |-> "mandate_context", project |-> "duplicate-display-name", path |-> "", cwd |-> <<>>],
      [tool |-> "project_tree", project |-> "duplicate-display-name", path |-> "", cwd |-> <<>>],
      [tool |-> "project_tree", project |-> "missing", path |-> "", cwd |-> <<>>],
      [tool |-> "file_info", project |-> "", path |-> "/ws/auth/auth/file_0.rs", cwd |-> <<>>],
      [tool |-> "file_info", project |-> "", path |-> "auth/file_0.rs", cwd |-> <<>>],
      [tool |-> "find_by_cwd", project |-> "", path |-> "", cwd |-> <<"ws", "boundary", "app", "src">>],
      [tool |-> "find_by_cwd", project |-> "", path |-> "", cwd |-> ApplicationSiblingCwd] }

VARIABLES req, status, result

vars == <<req, status, result>>

NameMatches(name) == {p \in Projects : ProjectName[p] = name}

IsPathPrefix(root, cwd) ==
    /\ Len(root) <= Len(cwd)
    /\ \A i \in 1..Len(root) : root[i] = cwd[i]

CwdMatches(cwd) == {p \in Projects : IsPathPrefix(ProjectPath[p], cwd)}

LongestCwdMatches(cwd) ==
    {p \in CwdMatches(cwd) :
        \A q \in CwdMatches(cwd) : Len(ProjectPath[q]) <= Len(ProjectPath[p])}

Init ==
    /\ req = NoReq
    /\ status = "idle"
    /\ result = {}

PickRequest(r) ==
    /\ status = "idle"
    /\ r \in Requests
    /\ req' = r
    /\ status' = "pending"
    /\ result' = {}

RespondNameScoped ==
    /\ status = "pending"
    /\ req.tool \in NameScopedTools
    /\ LET matches == NameMatches(req.project) IN
       /\ status' =
            IF Cardinality(matches) = 0 THEN "not_found"
            ELSE IF Cardinality(matches) = 1 THEN "ok"
            ELSE "ambiguous"
       /\ result' =
            IF Cardinality(matches) = 1 THEN {CHOOSE p \in matches : TRUE}
            ELSE {}
    /\ UNCHANGED req

RespondFileInfo ==
    /\ status = "pending"
    /\ req.tool = "file_info"
    /\ LET matches == {f \in Files : FileAbs[f] = req.path} IN
       /\ status' = IF Cardinality(matches) = 0 THEN "not_found" ELSE "ok"
       /\ result' = matches
    /\ UNCHANGED req

RespondListProjects ==
    /\ status = "pending"
    /\ req.tool = "list_projects"
    /\ status' = "ok"
    /\ result' = Projects
    /\ UNCHANGED req

RespondFindByCwd ==
    /\ status = "pending"
    /\ req.tool = "find_by_cwd"
    /\ LET matches == LongestCwdMatches(req.cwd) IN
       /\ status' = IF Cardinality(matches) = 0 THEN "not_found" ELSE "ok"
       /\ result' = matches
    /\ UNCHANGED req

Reset ==
    /\ status \in {"ok", "not_found", "ambiguous"}
    /\ req' = NoReq
    /\ status' = "idle"
    /\ result' = {}

Next ==
    \/ \E r \in Requests : PickRequest(r)
    \/ RespondNameScoped
    \/ RespondFileInfo
    \/ RespondListProjects
    \/ RespondFindByCwd
    \/ Reset

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ status \in {"idle", "pending", "ok", "not_found", "ambiguous"}
    /\ result \subseteq Projects \cup Files

NameScopedNoCrossProjectLeak ==
    /\ status = "ok"
    /\ req.tool \in NameScopedTools
    => \A p \in result : p \in Projects /\ ProjectName[p] = req.project

AmbiguousNamesFailClosed ==
    /\ status \in {"ok", "not_found", "ambiguous"}
    /\ req.tool \in NameScopedTools
    /\ Cardinality(NameMatches(req.project)) > 1
    => /\ status = "ambiguous"
       /\ result = {}

ExactPathOnly ==
    /\ status = "ok"
    /\ req.tool = "file_info"
    => \A f \in result : f \in Files /\ FileAbs[f] = req.path

ListProjectsPreservesProjectIdentity ==
    /\ status = "ok"
    /\ req.tool = "list_projects"
    => result = Projects

BoundarySafeCwd ==
    /\ status = "ok"
    /\ req.tool = "find_by_cwd"
    => \A p \in result : p \in Projects /\ IsPathPrefix(ProjectPath[p], req.cwd)

SiblingPrefixRejected ==
    /\ status \in {"ok", "not_found", "ambiguous"}
    /\ req.tool = "find_by_cwd"
    /\ req.cwd = ApplicationSiblingCwd
    => /\ status = "not_found"
       /\ result = {}

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        NameScopedNoCrossProjectLeak /\
        AmbiguousNamesFailClosed /\
        ExactPathOnly /\
        ListProjectsPreservesProjectIdentity /\
        BoundarySafeCwd /\
        SiblingPrefixRejected)

=============================================================================
