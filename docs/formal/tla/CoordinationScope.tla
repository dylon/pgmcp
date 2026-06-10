----------------------------- MODULE CoordinationScope -----------------------------
(***************************************************************************)
(* Request and trust boundary for the worktree-coordination *tool* family: *)
(*   coordinate_dependency_block, coordination_respond, suggest_worktree.   *)
(*                                                                         *)
(* This slice complements `WorktreeNegotiation.tla`, which models the      *)
(* coordination *protocol* state machine (R -> E exchange + the System     *)
(* git-scanner gatekeeper). Here we model only what crosses the MCP tool   *)
(* request boundary: fail-closed project resolution, the closed response   *)
(* vocabulary, the agent trust boundary on the `resolved` status, and the  *)
(* by-id row load for `suggest_worktree`.                                   *)
(*                                                                         *)
(* One request is processed per behavior (CircularDependenciesScope shape) *)
(* so the state space stays small and finite.                              *)
(*                                                                         *)
(* Hardened obligations (from `sota_helpers::project_id_or_err`):          *)
(*   - blank (post-trim) dependency / dependent project name -> rejected,  *)
(*     no coordination opened;                                             *)
(*   - DUPLICATE display-name -> rejected (cannot select an arbitrary row);*)
(*   - coordination_respond accepts only the closed vocab                  *)
(*     {accept, decline, moved} after trimming; everything else rejects;   *)
(*   - an agent (editor/requester) can NEVER set `resolved` — only the     *)
(*     System git-scanner observation resolves (mirrors                    *)
(*     WorktreeNegotiation's GatekeeperSafety);                            *)
(*   - suggest_worktree resolves exactly one project id, then loads its    *)
(*     row by that id (no duplicate / blank leak);                         *)
(*   - the respond candidate path (status='moved') is read-only w.r.t. the *)
(*     dependent's blocked state — it opens no unblock.                     *)
(***************************************************************************)

EXTENDS Naturals, Integers, Sequences, FiniteSets

Outcomes == {"ok", "rejected"}

\* The closed CoordinationStatus vocabulary an *agent* may set via
\* coordination_respond. `resolved` is intentionally absent: only the System
\* git scanner reaches it (CoordinationStatus::is_agent_settable() is false only
\* for Resolved).
AgentSettableStatuses == {"accept", "decline", "moved"}
\* The single status no agent path may ever produce.
SystemOnlyStatus == "resolved"

\* Two projects share the display name "dup" — a duplicate pair the resolver
\* must reject. "u" is the unique dependency; "d" is the unique dependent.
Projects ==
    { [id |-> 1, name |-> "u"],
      [id |-> 2, name |-> "d"],
      [id |-> 3, name |-> "dup"],
      [id |-> 4, name |-> "dup"] }

ProjectIds == {p.id : p \in Projects}

\* Tool tags so one Requests set can drive all three tools.
Tools == {"block", "respond", "suggest"}

\* `dependency` / `dependent` / `project` are name inputs subject to trim +
\* fail-closed resolution; `response` is the closed-vocab string for respond.
\* Inputs include blank, duplicate, unknown, padded-but-valid, and valid cases.
Requests ==
    { \* coordinate_dependency_block
      [id |-> 1, tool |-> "block", dependency |-> "u", dependent |-> "d",
       response |-> "", project |-> ""],
      [id |-> 2, tool |-> "block", dependency |-> "  ", dependent |-> "d",
       response |-> "", project |-> ""],
      [id |-> 3, tool |-> "block", dependency |-> "dup", dependent |-> "d",
       response |-> "", project |-> ""],
      [id |-> 4, tool |-> "block", dependency |-> "u", dependent |-> "dup",
       response |-> "", project |-> ""],
      [id |-> 5, tool |-> "block", dependency |-> "u", dependent |-> "  ",
       response |-> "", project |-> ""],
      \* coordination_respond
      [id |-> 6, tool |-> "respond", dependency |-> "", dependent |-> "",
       response |-> "accept", project |-> ""],
      [id |-> 7, tool |-> "respond", dependency |-> "", dependent |-> "",
       response |-> " decline ", project |-> ""],
      [id |-> 8, tool |-> "respond", dependency |-> "", dependent |-> "",
       response |-> "moved", project |-> ""],
      [id |-> 9, tool |-> "respond", dependency |-> "", dependent |-> "",
       response |-> "resolved", project |-> ""],
      [id |-> 10, tool |-> "respond", dependency |-> "", dependent |-> "",
       response |-> "yolo", project |-> ""],
      \* suggest_worktree
      [id |-> 11, tool |-> "suggest", dependency |-> "", dependent |-> "",
       response |-> "", project |-> "u"],
      [id |-> 12, tool |-> "suggest", dependency |-> "", dependent |-> "",
       response |-> "", project |-> "dup"],
      [id |-> 13, tool |-> "suggest", dependency |-> "", dependent |-> "",
       response |-> "", project |-> "  "],
      [id |-> 14, tool |-> "suggest", dependency |-> "", dependent |-> "",
       response |-> "", project |-> "missing"] }

RequestIds == {r.id : r \in Requests}

\* Trim model: only the synthetic padded inputs need trimming.
Trim(s) ==
    CASE s = "  "        -> ""
      [] s = " decline " -> "decline"
      [] OTHER           -> s

ProjectMatches(name) == {p \in Projects : p.name = name}

\* project_id_or_err: trim, reject blank, reject not-found, reject duplicate;
\* otherwise the unique id. 0 means "did not resolve to a unique id".
ResolveId(name) ==
    LET t == Trim(name) IN
    IF t = "" THEN 0
    ELSE IF Cardinality(ProjectMatches(t)) = 1
         THEN (CHOOSE p \in ProjectMatches(t) : TRUE).id
         ELSE 0

\* Whether a required name input fails closed (blank/unknown/duplicate).
NameRejected(name) == ResolveId(name) = 0

\* coordination_respond's optional dependent is only resolved when non-blank.
DependentResolves(r) ==
    Trim(r.dependent) = "" \/ ResolveId(r.dependent) # 0

RespondStatus(r) ==
    LET t == Trim(r.response) IN
    IF t \in AgentSettableStatuses THEN t ELSE "<<invalid>>"

RequestAccepted(r) ==
    CASE r.tool = "block" ->
            \* dependency must resolve; dependent (when present) must resolve too.
            ~NameRejected(r.dependency) /\ DependentResolves(r)
      [] r.tool = "respond" ->
            RespondStatus(r) \in AgentSettableStatuses
      [] r.tool = "suggest" ->
            ~NameRejected(r.project)
      [] OTHER -> FALSE

\* The project id a coordination is opened against (block) or a worktree is
\* suggested for (suggest). 0 for respond (it has no name input) or on reject.
ResolvedDependencyId(r) ==
    CASE r.tool = "block"   -> ResolveId(r.dependency)
      [] r.tool = "suggest" -> ResolveId(r.project)
      [] OTHER -> 0

ResponseFor(r) ==
    LET accepted == RequestAccepted(r) IN
    [ request_id    |-> r.id,
      tool          |-> r.tool,
      outcome       |-> IF accepted THEN "ok" ELSE "rejected",
      \* status is the recorded coordination status for respond, "" otherwise.
      status        |-> IF r.tool = "respond" /\ accepted
                        THEN RespondStatus(r) ELSE "",
      \* the project id any opened coordination / loaded row is keyed by.
      project_id    |-> IF accepted THEN ResolvedDependencyId(r) ELSE 0,
      \* coordination_opened: only block opens one, and only when accepted.
      coord_opened  |-> r.tool = "block" /\ accepted,
      \* row_loaded_by_id: suggest loads exactly one project row by id.
      row_loaded_id |-> IF r.tool = "suggest" /\ accepted
                        THEN ResolvedDependencyId(r) ELSE 0,
      \* unblock_opened: NO agent path here unblocks the dependent (the
      \* git-scanner gatekeeper does, elsewhere). Always FALSE.
      unblock_opened |-> FALSE ]

StatusDomain == AgentSettableStatuses \cup {SystemOnlyStatus, "<<invalid>>", ""}

ResponseRecord ==
    [ request_id: RequestIds,
      tool: Tools,
      outcome: Outcomes,
      status: StatusDomain,
      project_id: ProjectIds \cup {0},
      coord_opened: BOOLEAN,
      row_loaded_id: ProjectIds \cup {0},
      unblock_opened: BOOLEAN ]

VARIABLES req, response

vars == <<req, response>>

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests
    /\ response \in ResponseRecord
    /\ response.request_id = req.id

\* Blank/duplicate (or unknown) dependency / dependent project names fail closed,
\* opening no coordination and resolving no project id.
BlankOrDuplicateProjectRejects ==
    (req.tool = "block" /\ (NameRejected(req.dependency) \/ ~DependentResolves(req))) =>
        /\ response.outcome = "rejected"
        /\ response.coord_opened = FALSE
        /\ response.project_id = 0

\* suggest_worktree on a blank/duplicate/unknown project rejects and loads no row.
SuggestBlankOrDuplicateRejects ==
    (req.tool = "suggest" /\ NameRejected(req.project)) =>
        /\ response.outcome = "rejected"
        /\ response.row_loaded_id = 0

\* coordination_respond accepts ONLY the closed agent vocabulary (trimmed).
RespondOnlyClosedVocab ==
    req.tool = "respond" =>
        ((response.outcome = "ok") <=> (RespondStatus(req) \in AgentSettableStatuses))

\* TRUST BOUNDARY: no agent path through coordination_respond ever records the
\* System-only `resolved` status. (xref WorktreeNegotiation.GatekeeperSafety.)
AgentNeverResolves ==
    response.status # SystemOnlyStatus

\* An accepted respond records exactly the trimmed closed-vocab status it asked
\* for; a rejected respond records no status.
RespondStatusFaithful ==
    req.tool = "respond" =>
        /\ (response.outcome = "ok"  => response.status = RespondStatus(req))
        /\ (response.outcome = "rejected" => response.status = "")

\* suggest_worktree loads its project row by the single resolved id; an opened
\* coordination (block) is keyed by the same resolved dependency id.
RowLoadedByResolvedId ==
    /\ (req.tool = "suggest" /\ response.outcome = "ok") =>
          /\ response.row_loaded_id = response.project_id
          /\ response.row_loaded_id \in ProjectIds
    /\ (req.tool = "block" /\ response.coord_opened) =>
          response.project_id \in ProjectIds

\* The respond candidate path never opens an unblock of the dependent.
RespondPathNeverUnblocks ==
    response.unblock_opened = FALSE

\* Rejected requests perform no side effect: no coordination, no row, no unblock.
RejectedRequestsInert ==
    response.outcome = "rejected" =>
        /\ response.coord_opened = FALSE
        /\ response.row_loaded_id = 0
        /\ response.unblock_opened = FALSE

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        BlankOrDuplicateProjectRejects /\
        SuggestBlankOrDuplicateRejects /\
        RespondOnlyClosedVocab /\
        AgentNeverResolves /\
        RespondStatusFaithful /\
        RowLoadedByResolvedId /\
        RespondPathNeverUnblocks /\
        RejectedRequestsInert)

================================================================================
