----------------------------- MODULE ReindexSerializationScope -----------------------------
(***************************************************************************)
(* `reindex` request/serialization model.                                  *)
(*                                                                         *)
(* The tool validates an optional language token before acquiring the       *)
(* destructive-operation lock. A busy lock rejects without writes.          *)
(* Language reindex deletes only matching language files. Full reindex      *)
(* deletes chunks in bounded batches before deleting files, with daemon     *)
(* stopping checks between destructive phases.                              *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

RequestModes == {"full", "language_trim", "invalid_blank", "invalid_chars", "too_long"}
LockModes == {"free", "busy"}
StopModes == {"running", "stopping_before", "stopping_mid", "stopping_before_files"}
Outcomes == {"ok", "rejected", "cancelled"}
Reasons == {"none", "invalid_language", "lock_busy", "daemon_stopping"}
Languages == {"none", "rust"}

Requests ==
    { [id |-> 1, mode |-> "full", lock_mode |-> "free", stop_mode |-> "running"],
      [id |-> 2, mode |-> "language_trim", lock_mode |-> "free", stop_mode |-> "running"],
      [id |-> 3, mode |-> "invalid_blank", lock_mode |-> "free", stop_mode |-> "running"],
      [id |-> 4, mode |-> "invalid_chars", lock_mode |-> "free", stop_mode |-> "running"],
      [id |-> 5, mode |-> "too_long", lock_mode |-> "free", stop_mode |-> "running"],
      [id |-> 6, mode |-> "full", lock_mode |-> "busy", stop_mode |-> "running"],
      [id |-> 7, mode |-> "language_trim", lock_mode |-> "free", stop_mode |-> "stopping_before"],
      [id |-> 8, mode |-> "full", lock_mode |-> "free", stop_mode |-> "stopping_mid"],
      [id |-> 9, mode |-> "full", lock_mode |-> "free", stop_mode |-> "stopping_before_files"] }

RequestIds == {r.id : r \in Requests}

TotalChunks == 3
TotalFiles == 2
RustFiles == 1
OtherFiles == 1
BatchCap == 2

LanguageFor(r) ==
    IF r.mode = "language_trim" THEN "rust" ELSE "none"

LanguageValid(r) ==
    r.mode \in {"full", "language_trim"}

ReasonFor(r) ==
    CASE ~LanguageValid(r) -> "invalid_language"
      [] r.lock_mode = "busy" -> "lock_busy"
      [] r.stop_mode = "stopping_before" -> "daemon_stopping"
      [] OTHER -> "none"

LockAcquiredFor(r) ==
    LanguageValid(r) /\ r.lock_mode = "free"

OutcomeFor(r) ==
    CASE ReasonFor(r) = "invalid_language" -> "rejected"
      [] ReasonFor(r) = "lock_busy" -> "rejected"
      [] ReasonFor(r) = "daemon_stopping" -> "cancelled"
      [] r.stop_mode \in {"stopping_mid", "stopping_before_files"} -> "cancelled"
      [] OTHER -> "ok"

ChunksDeletedFor(r) ==
    CASE ReasonFor(r) # "none" -> 0
      [] r.mode = "language_trim" -> 1
      [] r.stop_mode = "stopping_mid" -> BatchCap
      [] OTHER -> TotalChunks

MaxChunkBatchFor(r) ==
    CASE ReasonFor(r) # "none" -> 0
      [] r.mode = "full" -> BatchCap
      [] OTHER -> 0

FilesDeletedFor(r) ==
    CASE ReasonFor(r) # "none" -> 0
      [] r.mode = "language_trim" -> RustFiles
      [] r.stop_mode \in {"stopping_mid", "stopping_before_files"} -> 0
      [] OTHER -> TotalFiles

OtherFilesRemainingFor(r) ==
    IF r.mode = "language_trim" /\ ReasonFor(r) = "none" THEN OtherFiles ELSE 0

ResponseFor(r) ==
    [ request_id |-> r.id,
      outcome |-> OutcomeFor(r),
      reason |-> ReasonFor(r),
      normalized_language |-> IF LanguageValid(r) THEN LanguageFor(r) ELSE "none",
      lock_acquired |-> LockAcquiredFor(r),
      chunks_deleted |-> ChunksDeletedFor(r),
      max_chunk_batch |-> MaxChunkBatchFor(r),
      files_deleted |-> FilesDeletedFor(r),
      other_files_remaining |-> OtherFilesRemainingFor(r),
      batch_cap |-> BatchCap,
      lock_held |-> FALSE ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      reason: Reasons,
      normalized_language: Languages,
      lock_acquired: BOOLEAN,
      chunks_deleted: 0..TotalChunks,
      max_chunk_batch: 0..BatchCap,
      files_deleted: 0..TotalFiles,
      other_files_remaining: 0..OtherFiles,
      batch_cap: 1..BatchCap,
      lock_held: BOOLEAN ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK == response \in ResponseRecord

InvalidLanguageNoWrite ==
    ~LanguageValid(req) =>
        /\ response.outcome = "rejected"
        /\ ~response.lock_acquired
        /\ response.chunks_deleted = 0
        /\ response.files_deleted = 0

BusyLockNoWrite ==
    req.lock_mode = "busy" /\ LanguageValid(req) =>
        /\ response.outcome = "rejected"
        /\ ~response.lock_acquired
        /\ response.chunks_deleted = 0
        /\ response.files_deleted = 0

NoConcurrentReindex ==
    response.lock_acquired => req.lock_mode = "free"

DaemonStoppingBeforeNoWrite ==
    req.stop_mode = "stopping_before" /\ LanguageValid(req) /\ req.lock_mode = "free" =>
        /\ response.outcome = "cancelled"
        /\ response.lock_acquired
        /\ response.chunks_deleted = 0
        /\ response.files_deleted = 0

LanguageNormalized ==
    req.mode = "language_trim" /\ response.lock_acquired =>
        response.normalized_language = "rust"

LanguageModeScoped ==
    req.mode = "language_trim" /\ response.lock_acquired /\ response.outcome = "ok" =>
        /\ response.files_deleted = RustFiles
        /\ response.other_files_remaining = OtherFiles

FullDeleteChunksBeforeFiles ==
    req.mode = "full" /\ response.files_deleted > 0 =>
        response.chunks_deleted = TotalChunks

CancellationStopsBeforeFileDelete ==
    req.stop_mode \in {"stopping_mid", "stopping_before_files"} =>
        /\ response.outcome = "cancelled"
        /\ response.files_deleted = 0

BatchedDeleteBounded ==
    response.max_chunk_batch <= BatchCap /\ response.batch_cap = BatchCap

LockReleased ==
    response.lock_held = FALSE

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidLanguageNoWrite /\
        BusyLockNoWrite /\
        NoConcurrentReindex /\
        DaemonStoppingBeforeNoWrite /\
        LanguageNormalized /\
        LanguageModeScoped /\
        FullDeleteChunksBeforeFiles /\
        CancellationStopsBeforeFileDelete /\
        BatchedDeleteBounded /\
        LockReleased)

================================================================================
