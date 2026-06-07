----------------------------- MODULE ExperimentRenderLedgerScope -----------------------------
(***************************************************************************)
(* `experiment_render_ledger` lookup/path/write model.                     *)
(*                                                                         *)
(* The tool renders the structured experiment record to markdown. Dry runs  *)
(* return content and write nothing; non-dry runs write exactly one ledger  *)
(* file under a configured relative ledger directory. Unsafe lookup inputs, *)
(* unsafe ledger directories, and unsafe stored slugs reject. Writes are    *)
(* modeled as atomic publishes, so no partial ledger is visible.            *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

LookupModes == {"id_valid", "id_invalid", "slug_valid_trimmed", "none"}
DirModes == {"safe_relative", "absolute", "parent", "blank"}
StoredSlugModes == {"safe", "slash", "parent", "blank"}
Outcomes == {"ok", "rejected"}

Requests ==
    { [id |-> 1, lookup |-> "id_valid", dir |-> "safe_relative",
       stored_slug |-> "safe", dry_run |-> TRUE],
      [id |-> 2, lookup |-> "id_valid", dir |-> "safe_relative",
       stored_slug |-> "safe", dry_run |-> FALSE],
      [id |-> 3, lookup |-> "slug_valid_trimmed", dir |-> "safe_relative",
       stored_slug |-> "safe", dry_run |-> TRUE],
      [id |-> 4, lookup |-> "id_invalid", dir |-> "safe_relative",
       stored_slug |-> "safe", dry_run |-> FALSE],
      [id |-> 5, lookup |-> "none", dir |-> "safe_relative",
       stored_slug |-> "safe", dry_run |-> FALSE],
      [id |-> 6, lookup |-> "id_valid", dir |-> "parent",
       stored_slug |-> "safe", dry_run |-> TRUE],
      [id |-> 7, lookup |-> "id_valid", dir |-> "absolute",
       stored_slug |-> "safe", dry_run |-> FALSE],
      [id |-> 8, lookup |-> "id_valid", dir |-> "safe_relative",
       stored_slug |-> "slash", dry_run |-> TRUE],
      [id |-> 9, lookup |-> "id_valid", dir |-> "safe_relative",
       stored_slug |-> "parent", dry_run |-> FALSE],
      [id |-> 10, lookup |-> "id_valid", dir |-> "blank",
       stored_slug |-> "safe", dry_run |-> FALSE] }

RequestIds == {r.id : r \in Requests}

LookupOK(r) == r.lookup \in {"id_valid", "slug_valid_trimmed"}
DirOK(r) == r.dir = "safe_relative"
StoredSlugOK(r) == r.stored_slug = "safe"
Accepted(r) == LookupOK(r) /\ DirOK(r) /\ StoredSlugOK(r)

ResponseFor(r) ==
    [ request_id |-> r.id,
      outcome |-> IF Accepted(r) THEN "ok" ELSE "rejected",
      content_returned |-> Accepted(r) /\ r.dry_run,
      file_written |-> Accepted(r) /\ ~r.dry_run,
      path_inside_ledger_dir |-> Accepted(r),
      filename_uses_safe_slug |-> Accepted(r),
      atomic_publish |-> Accepted(r) /\ ~r.dry_run,
      partial_file_visible |-> FALSE,
      db_written |-> FALSE,
      lock_held |-> FALSE ]

VARIABLES req, response

vars == <<req, response>>

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      content_returned: BOOLEAN,
      file_written: BOOLEAN,
      path_inside_ledger_dir: BOOLEAN,
      filename_uses_safe_slug: BOOLEAN,
      atomic_publish: BOOLEAN,
      partial_file_visible: BOOLEAN,
      db_written: BOOLEAN,
      lock_held: BOOLEAN ]

Init ==
    /\ req \in Requests
    /\ response = ResponseFor(req)

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    response \in ResponseRecord

InvalidInputsReject ==
    ~Accepted(req) =>
        /\ response.outcome = "rejected"
        /\ ~response.content_returned
        /\ ~response.file_written

TrimmedSlugLookupAccepted ==
    req.lookup = "slug_valid_trimmed" => response.outcome = "ok"

DryRunWritesNothing ==
    req.dry_run => ~response.file_written

DryRunReturnsContent ==
    Accepted(req) /\ req.dry_run => response.content_returned

WriteModePublishesOneContainedFile ==
    Accepted(req) /\ ~req.dry_run =>
        /\ response.file_written
        /\ response.path_inside_ledger_dir
        /\ response.filename_uses_safe_slug

AtomicNoPartialFile ==
    /\ response.atomic_publish = (Accepted(req) /\ ~req.dry_run)
    /\ ~response.partial_file_visible

NoDbMutationNoLock ==
    /\ ~response.db_written
    /\ ~response.lock_held

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidInputsReject /\
        TrimmedSlugLookupAccepted /\
        DryRunWritesNothing /\
        DryRunReturnsContent /\
        WriteModePublishesOneContainedFile /\
        AtomicNoPartialFile /\
        NoDbMutationNoLock)

================================================================================
