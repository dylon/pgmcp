-------------------------- MODULE SendSyncViolationsScan --------------------------
(***************************************************************************)
(* `send_sync_violations` request-boundary and bounded scan model.         *)
(*                                                                         *)
(* The tool resolves a unique project id, clamps the match limit, streams    *)
(* Rust files in id order, stops after the normalized limit, and returns     *)
(* unsafe-effect enrichment scoped to the same project id.                  *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

MaxLimit == 200
DefaultLimit == 50

Files == {"a.rs", "b.rs", "notes.txt", "other.rs"}
Projects == {1, 2}
Outcomes == {"ok", "rejected"}
Reasons == {"none", "blank_project", "unknown_project", "duplicate_project"}

Requests ==
    { [ id |-> 1, raw_project |-> " p5-ss ", project_case |-> "unique",
        limit |-> DefaultLimit ],
      [ id |-> 2, raw_project |-> "   ", project_case |-> "unique",
        limit |-> DefaultLimit ],
      [ id |-> 3, raw_project |-> "p5-ss", project_case |-> "duplicate",
        limit |-> DefaultLimit ],
      [ id |-> 4, raw_project |-> "missing", project_case |-> "unknown",
        limit |-> DefaultLimit ],
      [ id |-> 5, raw_project |-> "p5-ss", project_case |-> "unique",
        limit |-> -10 ],
      [ id |-> 6, raw_project |-> "p5-ss", project_case |-> "unique",
        limit |-> 999 ] }

RequestIds == {r.id : r \in Requests}

NormalizeProject(raw) ==
    CASE raw = " p5-ss " -> "p5-ss"
      [] raw = "   " -> ""
      [] OTHER -> raw

ResolvedProjectId(r) ==
    IF r.project_case = "unique" THEN 1 ELSE 0

ReasonFor(r) ==
    CASE NormalizeProject(r.raw_project) = "" -> "blank_project"
      [] r.project_case = "unknown" -> "unknown_project"
      [] r.project_case = "duplicate" -> "duplicate_project"
      [] OTHER -> "none"

LimitFor(r) ==
    IF r.limit < 1 THEN 1
    ELSE IF r.limit > MaxLimit THEN MaxLimit
    ELSE r.limit

FileProject(f) ==
    CASE f = "other.rs" -> 2
      [] OTHER -> 1

FileLang(f) ==
    CASE f = "notes.txt" -> "text"
      [] OTHER -> "rust"

FileHits(f) ==
    CASE f = "a.rs" -> 3
      [] f = "b.rs" -> 1
      [] f = "notes.txt" -> 1
      [] f = "other.rs" -> 1

RustFilesFor(r) ==
    {f \in Files : FileProject(f) = ResolvedProjectId(r) /\ FileLang(f) = "rust"}

AvailableHits(r) ==
    IF ReasonFor(r) = "none"
    THEN FileHits("a.rs") + FileHits("b.rs")
    ELSE 0

Min(a, b) == IF a <= b THEN a ELSE b

RowsScannedFor(r) ==
    IF ReasonFor(r) # "none" THEN 0
    ELSE IF LimitFor(r) <= FileHits("a.rs") THEN 1
    ELSE Cardinality(RustFilesFor(r))

ResponseFor(r) ==
    LET reason == ReasonFor(r) IN
        [ request_id |-> r.id,
          outcome |-> IF reason = "none" THEN "ok" ELSE "rejected",
          reason |-> reason,
          project |-> NormalizeProject(r.raw_project),
          project_id |-> IF reason = "none" THEN ResolvedProjectId(r) ELSE 0,
          limit |-> IF reason = "none" THEN LimitFor(r) ELSE 0,
          rust_files |-> IF reason = "none" THEN RustFilesFor(r) ELSE {},
          result_count |-> IF reason = "none"
                           THEN Min(AvailableHits(r), LimitFor(r))
                           ELSE 0,
          rows_scanned |-> RowsScannedFor(r),
          unsafe_symbol_project_id |-> IF reason = "none" THEN ResolvedProjectId(r) ELSE 0,
          writes |-> 0,
          locks |-> 0 ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      reason: Reasons,
      project: {"", "p5-ss", "missing"},
      project_id: 0..2,
      limit: 0..MaxLimit,
      rust_files: SUBSET Files,
      result_count: 0..MaxLimit,
      rows_scanned: 0..Cardinality(Files),
      unsafe_symbol_project_id: 0..2,
      writes: 0..0,
      locks: 0..0 ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK == response \in ResponseRecord

InvalidRequestsReject ==
    ReasonFor(req) # "none" => response.outcome = "rejected"

SuccessfulRequestsResolvedUniqueProject ==
    response.outcome = "ok" =>
        /\ response.project = "p5-ss"
        /\ response.project_id = 1
        /\ req.project_case = "unique"

LimitBound ==
    response.outcome = "ok" =>
        /\ response.limit \in 1..MaxLimit
        /\ response.result_count <= response.limit

RustFilesOnlyAndProjectScoped ==
    response.outcome = "ok" =>
        \A f \in response.rust_files :
            /\ FileProject(f) = response.project_id
            /\ FileLang(f) = "rust"

StreamingStopsAtLimit ==
    response.outcome = "ok" =>
        /\ response.rows_scanned <= Cardinality(response.rust_files)
        /\ response.limit <= FileHits("a.rs") => response.rows_scanned = 1

UnsafeSymbolsUseSameProject ==
    response.outcome = "ok" =>
        response.unsafe_symbol_project_id = response.project_id

ReadOnlyNoLocks ==
    /\ response.writes = 0
    /\ response.locks = 0

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidRequestsReject /\
        SuccessfulRequestsResolvedUniqueProject /\
        LimitBound /\
        RustFilesOnlyAndProjectScoped /\
        StreamingStopsAtLimit /\
        UnsafeSymbolsUseSameProject /\
        ReadOnlyNoLocks)

=============================================================================
