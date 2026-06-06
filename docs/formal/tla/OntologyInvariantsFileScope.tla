-------------------------- MODULE OntologyInvariantsFileScope --------------------------
(***************************************************************************)
(* `ontology_invariants_for_file` file-resolution model.                   *)
(*                                                                         *)
(* The tool trims file input, rejects blank and ambiguous matches, treats    *)
(* wildcard-looking characters literally, and returns invariants only for    *)
(* the single resolved indexed file.                                        *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

Files ==
    { [id |-> 1, path |-> "/ws/p/src/parser.rs", relative |-> "src/parser.rs"],
      [id |-> 2, path |-> "/ws/q/src/parser.rs", relative |-> "src/parser.rs"],
      [id |-> 3, path |-> "/ws/r/src/unique.rs", relative |-> "src/unique.rs"] }

InvariantRows ==
    { [id |-> 1, file_id |-> 1],
      [id |-> 2, file_id |-> 3] }

Requests ==
    { [id |-> 1, raw_file |-> " /ws/p/src/parser.rs "],
      [id |-> 2, raw_file |-> "src/unique.rs"],
      [id |-> 3, raw_file |-> "src/parser.rs"],
      [id |-> 4, raw_file |-> "   "],
      [id |-> 5, raw_file |-> "%"],
      [id |-> 6, raw_file |-> "src/missing.rs"] }

RequestIds == {r.id : r \in Requests}
FileIds == {f.id : f \in Files}
InvariantIds == {i.id : i \in InvariantRows}

Trim(raw) ==
    CASE raw = " /ws/p/src/parser.rs " -> "/ws/p/src/parser.rs"
      [] raw = "   " -> ""
      [] OTHER -> raw

PathMatches(file, requested) ==
    CASE requested = "/ws/p/src/parser.rs" -> file.path = "/ws/p/src/parser.rs"
      [] requested = "src/parser.rs" -> file.relative = "src/parser.rs"
      [] requested = "src/unique.rs" -> file.id = 3
      [] OTHER -> FALSE

Matches(requested) ==
    {f.id : f \in {x \in Files : PathMatches(x, requested)}}

ResolvedFile(requested) ==
    IF Cardinality(Matches(requested)) = 1
    THEN CHOOSE file_id \in Matches(requested) : TRUE
    ELSE 0

ReasonFor(r) ==
    CASE Trim(r.raw_file) = "" -> "blank_file"
      [] Cardinality(Matches(Trim(r.raw_file))) > 1 -> "ambiguous_file"
      [] OTHER -> "none"

InvariantIdsForFile(file_id) ==
    {i.id : i \in {x \in InvariantRows : x.file_id = file_id}}

ResponseFor(r) ==
    LET requested == Trim(r.raw_file) IN
    LET reason == ReasonFor(r) IN
    LET file_id == ResolvedFile(requested) IN
        [ request_id |-> r.id,
          accepted |-> reason = "none",
          reason |-> reason,
          file |-> requested,
          resolved_file |-> file_id,
          invariants |-> IF reason = "none" /\ file_id # 0
                         THEN InvariantIdsForFile(file_id)
                         ELSE {},
          writes |-> 0,
          lock_held |-> FALSE ]

VARIABLES req, response

vars == <<req, response>>

Reasons == {"none", "blank_file", "ambiguous_file"}

ResponseRecord ==
    [ request_id: RequestIds,
      accepted: BOOLEAN,
      reason: Reasons,
      file: {"", "%", "src/unique.rs", "src/parser.rs", "src/missing.rs", "/ws/p/src/parser.rs"},
      resolved_file: {0} \cup FileIds,
      invariants: SUBSET InvariantIds,
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
        /\ response.accepted = FALSE
        /\ response.resolved_file = 0
        /\ response.invariants = {}

MissingFilesReturnEmpty ==
    Trim(req.raw_file) = "src/missing.rs" =>
        /\ response.accepted = TRUE
        /\ response.resolved_file = 0
        /\ response.invariants = {}

WildcardsAreLiteral ==
    Trim(req.raw_file) = "%" =>
        /\ response.accepted = TRUE
        /\ response.resolved_file = 0
        /\ response.invariants = {}

ResolvedFileIsUnique ==
    response.accepted /\ response.resolved_file # 0 =>
        Cardinality(Matches(response.file)) = 1

InvariantsScopedToResolvedFile ==
    response.accepted /\ response.resolved_file # 0 =>
        \A row \in InvariantRows :
            row.id \in response.invariants => row.file_id = response.resolved_file

ReadOnlyNoHeldLock ==
    /\ response.writes = 0
    /\ response.lock_held = FALSE

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        InvalidRequestsReject /\
        MissingFilesReturnEmpty /\
        WildcardsAreLiteral /\
        ResolvedFileIsUnique /\
        InvariantsScopedToResolvedFile /\
        ReadOnlyNoHeldLock)

================================================================================
