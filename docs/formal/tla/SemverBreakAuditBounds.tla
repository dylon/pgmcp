---------------------------- MODULE SemverBreakAuditBounds ----------------------------
(***************************************************************************)
(* `semver_break_audit` request and scan boundary.                         *)
(*                                                                         *)
(* The tool resolves a normalized project, rejects overlarge public API     *)
(* snapshots, scans a bounded recent commit-window from git_commit_chunks   *)
(* `content`, and reports a bounded removed/renamed public API list. Rename *)
(* candidate selection streams over the inherited DAWG/transducer query.    *)
(*                                                                         *)
(* Verified:                                                               *)
(*   - invalid projects reject before commit/public scans;                  *)
(*   - commit windows clamp to 1..=1000 and output limits to 1..=250;       *)
(*   - public-symbol snapshots above the cap reject without commit scans;   *)
(*   - only current-schema `content` commit chunks are read;                *)
(*   - removed/renamed output never exceeds the effective limit;            *)
(*   - rename selection keeps at most one candidate in memory;              *)
(*   - execution is read-only.                                             *)
(***************************************************************************)

EXTENDS Naturals

MaxWindow == 1000
MaxLimit == 250
MaxPublicSymbols == 50000
MaxDistance == 2

Reasons == {"none", "blank_project", "missing_project", "duplicate_project", "too_many_public_symbols"}

NoReq ==
    [ id |-> 0,
      project |-> "",
      window |-> 50,
      limit |-> 50,
      public_symbols |-> 0,
      historical_removed |-> 0,
      schema |-> "content",
      rename_match |-> FALSE ]

Requests ==
    { [ id |-> 1, project |-> " p7-sba ", window |-> 0, limit |-> 0,
        public_symbols |-> 1, historical_removed |-> 1, schema |-> "content",
        rename_match |-> TRUE ],
      [ id |-> 2, project |-> "   ", window |-> 50, limit |-> 50,
        public_symbols |-> 1, historical_removed |-> 1, schema |-> "content",
        rename_match |-> TRUE ],
      [ id |-> 3, project |-> "missing", window |-> 50, limit |-> 50,
        public_symbols |-> 1, historical_removed |-> 1, schema |-> "content",
        rename_match |-> TRUE ],
      [ id |-> 4, project |-> "dupe", window |-> 50, limit |-> 50,
        public_symbols |-> 1, historical_removed |-> 1, schema |-> "content",
        rename_match |-> TRUE ],
      [ id |-> 5, project |-> "p7-sba", window |-> 5000, limit |-> 500,
        public_symbols |-> 100, historical_removed |-> 400, schema |-> "content",
        rename_match |-> TRUE ],
      [ id |-> 6, project |-> "p7-sba", window |-> 50, limit |-> 50,
        public_symbols |-> 50001, historical_removed |-> 1, schema |-> "content",
        rename_match |-> TRUE ],
      [ id |-> 7, project |-> "p7-sba", window |-> 50, limit |-> 50,
        public_symbols |-> 1, historical_removed |-> 0, schema |-> "content",
        rename_match |-> FALSE ] }

TrimProject(p) ==
    CASE p = " p7-sba " -> "p7-sba"
      [] p = "   " -> ""
      [] OTHER -> p

ProjectState(p) ==
    CASE TrimProject(p) = "" -> "blank_project"
      [] TrimProject(p) = "missing" -> "missing_project"
      [] TrimProject(p) = "dupe" -> "duplicate_project"
      [] OTHER -> "ok"

ClampWindow(n) ==
    IF n < 1 THEN 1 ELSE IF n > MaxWindow THEN MaxWindow ELSE n

ClampLimit(n) ==
    IF n < 1 THEN 1 ELSE IF n > MaxLimit THEN MaxLimit ELSE n

Min(a, b) == IF a < b THEN a ELSE b

Reject(reason, lookups, counted) ==
    [ rejected |-> TRUE,
      reason |-> reason,
      normalized_project |-> "",
      effective_window |-> 50,
      effective_limit |-> 50,
      project_lookups |-> lookups,
      public_symbol_counted |-> counted,
      public_snapshot_loaded |-> FALSE,
      commit_chunks_read |-> 0,
      commit_chunk_column |-> "none",
      removed_reported |-> 0,
      best_buffer_size |-> 0,
      max_distance |-> MaxDistance,
      writes |-> 0 ]

Accept(r) ==
    [ rejected |-> FALSE,
      reason |-> "none",
      normalized_project |-> TrimProject(r.project),
      effective_window |-> ClampWindow(r.window),
      effective_limit |-> ClampLimit(r.limit),
      project_lookups |-> 1,
      public_symbol_counted |-> TRUE,
      public_snapshot_loaded |-> TRUE,
      commit_chunks_read |-> ClampWindow(r.window),
      commit_chunk_column |-> r.schema,
      removed_reported |-> Min(r.historical_removed, ClampLimit(r.limit)),
      best_buffer_size |-> IF r.historical_removed = 0 \/ ~r.rename_match THEN 0 ELSE 1,
      max_distance |-> MaxDistance,
      writes |-> 0 ]

Evaluate(r) ==
    IF ProjectState(r.project) # "ok" THEN Reject(ProjectState(r.project), 1, FALSE)
    ELSE IF r.public_symbols > MaxPublicSymbols THEN Reject("too_many_public_symbols", 1, TRUE)
    ELSE Accept(r)

NoResp ==
    [ rejected |-> FALSE,
      reason |-> "none",
      normalized_project |-> "",
      effective_window |-> 50,
      effective_limit |-> 50,
      project_lookups |-> 0,
      public_symbol_counted |-> FALSE,
      public_snapshot_loaded |-> FALSE,
      commit_chunks_read |-> 0,
      commit_chunk_column |-> "none",
      removed_reported |-> 0,
      best_buffer_size |-> 0,
      max_distance |-> MaxDistance,
      writes |-> 0 ]

VARIABLES req, resp

vars == <<req, resp>>

Init ==
    /\ req = NoReq
    /\ resp = NoResp

Handle(r) ==
    /\ req = NoReq
    /\ r \in Requests
    /\ req' = r
    /\ resp' = Evaluate(r)

Done ==
    /\ req # NoReq
    /\ UNCHANGED vars

Next ==
    \/ \E r \in Requests : Handle(r)
    \/ Done

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ req \in Requests \cup {NoReq}
    /\ resp.rejected \in BOOLEAN
    /\ resp.reason \in Reasons
    /\ resp.normalized_project \in {"", "p7-sba"}
    /\ resp.effective_window \in 1..MaxWindow
    /\ resp.effective_limit \in 1..MaxLimit
    /\ resp.project_lookups \in 0..1
    /\ resp.public_symbol_counted \in BOOLEAN
    /\ resp.public_snapshot_loaded \in BOOLEAN
    /\ resp.commit_chunks_read \in 0..MaxWindow
    /\ resp.commit_chunk_column \in {"none", "content", "chunk_text"}
    /\ resp.removed_reported \in 0..MaxLimit
    /\ resp.best_buffer_size \in 0..1
    /\ resp.max_distance = MaxDistance
    /\ resp.writes = 0

InvalidProjectsDoNotScan ==
    req # NoReq /\ resp.reason \in {"blank_project", "missing_project", "duplicate_project"} =>
        /\ resp.public_symbol_counted = FALSE
        /\ resp.public_snapshot_loaded = FALSE
        /\ resp.commit_chunks_read = 0

PublicSymbolCapPreventsCommitScan ==
    req # NoReq /\ resp.reason = "too_many_public_symbols" =>
        /\ resp.public_symbol_counted = TRUE
        /\ resp.public_snapshot_loaded = FALSE
        /\ resp.commit_chunks_read = 0

EffectiveBoundsHold ==
    req # NoReq /\ ~resp.rejected =>
        /\ resp.effective_window <= MaxWindow
        /\ resp.effective_limit <= MaxLimit
        /\ resp.commit_chunks_read = resp.effective_window
        /\ resp.removed_reported <= resp.effective_limit

UsesCurrentCommitChunkSchema ==
    req # NoReq /\ ~resp.rejected => resp.commit_chunk_column = "content"

StreamingBestSelection ==
    req # NoReq => resp.best_buffer_size <= 1

ReadOnly ==
    req # NoReq => resp.writes = 0

=============================================================================
