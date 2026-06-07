-------------------------- MODULE ConcurrencyAuditScopes --------------------------
(***************************************************************************)
(* Boundary model for the `lockset_races` and `blocking_in_async` MCP       *)
(* tools. The production tools are read-only concurrency-audit scanners:    *)
(* they resolve one project, stream file content, cap caller limits, and    *)
(* enrich results from project-scoped symbol/effect rows.                   *)
(***************************************************************************)

EXTENDS Naturals, FiniteSets, TLC

MaxLimit == 1000

Projects == {1, 2, 3, 4}

ProjectName ==
    ( 1 :> "target"
   @@ 2 :> "other"
   @@ 3 :> "duplicate"
   @@ 4 :> "duplicate" )

Files == {101, 102, 201, 202}
FileProject == (101 :> 1 @@ 102 :> 1 @@ 201 :> 2 @@ 202 :> 2)

LockPatternFiles == {101, 201}
BlockingPatternFiles == {102, 202}

Symbols == {
    [id |-> "target-lock", project |-> 1, effects |-> {"lock_acquire"}],
    [id |-> "target-blocking", project |-> 1, effects |-> {"async", "blocking_io"}],
    [id |-> "other-panic", project |-> 2, effects |-> {"may_panic"}],
    [id |-> "other-blocking", project |-> 2, effects |-> {"async", "blocking_io"}]
}

NoReq == [tool |-> "none", project |-> "", limit |-> 0]

Requests == {
    [tool |-> "lockset_races", project |-> " target ", limit |-> 5000],
    [tool |-> "lockset_races", project |-> "duplicate", limit |-> 10],
    [tool |-> "blocking_in_async", project |-> "target", limit |-> 0 - 50],
    [tool |-> "blocking_in_async", project |-> "", limit |-> 50],
    [tool |-> "blocking_in_async", project |-> "missing", limit |-> 50]
}

VARIABLES req, status, resolved_project, effective_limit, regex_files,
          effect_rows, resident_content_files, writes, locks_held

vars == <<req, status, resolved_project, effective_limit, regex_files,
          effect_rows, resident_content_files, writes, locks_held>>

TrimProject(raw) ==
    CASE raw = " target " -> "target"
      [] OTHER -> raw

ProjectMatches(name) == {p \in Projects : ProjectName[p] = name}

ClampLimit(n) ==
    IF n < 1 THEN 1 ELSE IF n > MaxLimit THEN MaxLimit ELSE n

PatternFilesFor(tool) ==
    IF tool = "lockset_races" THEN LockPatternFiles ELSE BlockingPatternFiles

FileResults(tool, project) ==
    {f \in PatternFilesFor(tool) : FileProject[f] = project}

LocksetEffects(project) ==
    LET project_symbols == {s \in Symbols : s["project"] = project} IN
    {s["effects"] : s \in project_symbols}

EffectNames(project) ==
    UNION LocksetEffects(project)

BlockingSymbols(project) ==
    LET project_symbols ==
        {s \in Symbols :
            /\ s["project"] = project
            /\ "async" \in s["effects"]
            /\ "blocking_io" \in s["effects"]} IN
    {s["id"] : s \in project_symbols}

EffectRowsFor(tool, project) ==
    IF tool = "lockset_races"
    THEN EffectNames(project)
    ELSE BlockingSymbols(project)

Init ==
    /\ req = NoReq
    /\ status = "idle"
    /\ resolved_project = 0
    /\ effective_limit = 0
    /\ regex_files = {}
    /\ effect_rows = {}
    /\ resident_content_files = 0
    /\ writes = {}
    /\ locks_held = {}

PickRequest(r) ==
    /\ status = "idle"
    /\ r \in Requests
    /\ req' = r
    /\ status' = "pending"
    /\ resolved_project' = 0
    /\ effective_limit' = 0
    /\ regex_files' = {}
    /\ effect_rows' = {}
    /\ resident_content_files' = 0
    /\ writes' = {}
    /\ locks_held' = {}

Respond ==
    /\ status = "pending"
    /\ LET name == TrimProject(req.project) IN
       LET matches == ProjectMatches(name) IN
       IF name = "" THEN
          /\ status' = "invalid"
          /\ resolved_project' = 0
          /\ effective_limit' = 0
          /\ regex_files' = {}
          /\ effect_rows' = {}
          /\ resident_content_files' = 0
       ELSE IF Cardinality(matches) = 0 THEN
          /\ status' = "not_found"
          /\ resolved_project' = 0
          /\ effective_limit' = 0
          /\ regex_files' = {}
          /\ effect_rows' = {}
          /\ resident_content_files' = 0
       ELSE IF Cardinality(matches) > 1 THEN
          /\ status' = "ambiguous"
          /\ resolved_project' = 0
          /\ effective_limit' = 0
          /\ regex_files' = {}
          /\ effect_rows' = {}
          /\ resident_content_files' = 0
       ELSE
          /\ status' = "ok"
          /\ resolved_project' = CHOOSE p \in matches : TRUE
          /\ effective_limit' = ClampLimit(req.limit)
          /\ regex_files' = FileResults(req.tool, resolved_project')
          /\ effect_rows' = EffectRowsFor(req.tool, resolved_project')
          /\ resident_content_files' =
                IF Cardinality(FileResults(req.tool, resolved_project')) = 0 THEN 0 ELSE 1
    /\ writes' = {}
    /\ locks_held' = {}
    /\ UNCHANGED req

Reset ==
    /\ status \in {"ok", "invalid", "not_found", "ambiguous"}
    /\ req' = NoReq
    /\ status' = "idle"
    /\ resolved_project' = 0
    /\ effective_limit' = 0
    /\ regex_files' = {}
    /\ effect_rows' = {}
    /\ resident_content_files' = 0
    /\ writes' = {}
    /\ locks_held' = {}

Next ==
    \/ \E r \in Requests : PickRequest(r)
    \/ Respond
    \/ Reset

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ status \in {"idle", "pending", "ok", "invalid", "not_found", "ambiguous"}
    /\ resolved_project \in Projects \cup {0}
    /\ effective_limit \in 0..MaxLimit
    /\ regex_files \subseteq Files
    /\ effect_rows \subseteq {
        "lock_acquire", "async", "blocking_io", "may_panic",
        "target-lock", "target-blocking", "other-panic", "other-blocking"}
    /\ resident_content_files \in 0..1
    /\ writes = {}
    /\ locks_held = {}

InvalidOrAmbiguousNoScan ==
    status \in {"invalid", "not_found", "ambiguous"}
    => /\ resolved_project = 0
       /\ regex_files = {}
       /\ effect_rows = {}
       /\ resident_content_files = 0

LimitBounded ==
    status = "ok" => effective_limit \in 1..MaxLimit

RegexFilesScoped ==
    status = "ok"
    => \A f \in regex_files : FileProject[f] = resolved_project

LocksetEffectsScoped ==
    /\ status = "ok"
    /\ req.tool = "lockset_races"
    => effect_rows \subseteq EffectNames(resolved_project)

BlockingEffectsScoped ==
    /\ status = "ok"
    /\ req.tool = "blocking_in_async"
    => effect_rows \subseteq BlockingSymbols(resolved_project)

StreamingScanBound ==
    resident_content_files <= 1

ReadOnlyAndNoLocksHeld ==
    /\ writes = {}
    /\ locks_held = {}

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidOrAmbiguousNoScan /\
        LimitBounded /\
        RegexFilesScoped /\
        LocksetEffectsScoped /\
        BlockingEffectsScoped /\
        StreamingScanBound /\
        ReadOnlyAndNoLocksHeld)

=============================================================================
