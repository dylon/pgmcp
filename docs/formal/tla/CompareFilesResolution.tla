----------------------------- MODULE CompareFilesResolution -----------------------------
(***************************************************************************)
(* `compare_files` reference-resolution and alignment boundary.            *)
(*                                                                         *)
(* File references may be absolute paths or `project:relative_path`. The    *)
(* project-qualified form must fail closed when a display name is           *)
(* ambiguous. Once both references resolve, the reported chunk alignment    *)
(* must be a one-to-one subset of chunk pairs belonging to those files.     *)
(***************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

Projects ==
    { [id |-> 1, name |-> "alpha"],
      [id |-> 2, name |-> "duplicate"],
      [id |-> 3, name |-> "duplicate"] }

Files ==
    { [id |-> 10, project_id |-> 1, path |-> "/ws/alpha/src/a.rs", relative_path |-> "src/a.rs"],
      [id |-> 20, project_id |-> 1, path |-> "/ws/alpha/src/b.rs", relative_path |-> "src/b.rs"],
      [id |-> 30, project_id |-> 2, path |-> "/ws/dup-left/src/lib.rs", relative_path |-> "src/lib.rs"],
      [id |-> 40, project_id |-> 3, path |-> "/ws/dup-right/src/lib.rs", relative_path |-> "src/lib.rs"] }

Chunks ==
    { [id |-> 100, file_id |-> 10],
      [id |-> 101, file_id |-> 10],
      [id |-> 200, file_id |-> 20],
      [id |-> 201, file_id |-> 20],
      [id |-> 300, file_id |-> 30],
      [id |-> 400, file_id |-> 40] }

Refs ==
    { [kind |-> "qualified", project |-> "alpha", relative_path |-> "src/a.rs", path |-> ""],
      [kind |-> "qualified", project |-> "alpha", relative_path |-> "src/b.rs", path |-> ""],
      [kind |-> "qualified", project |-> "duplicate", relative_path |-> "src/lib.rs", path |-> ""],
      [kind |-> "absolute", project |-> "", relative_path |-> "", path |-> "/ws/alpha/src/a.rs"] }

NoRef == [kind |-> "absolute", project |-> "", relative_path |-> "", path |-> ""]
NoReq == [id |-> 0, file_a |-> NoRef, file_b |-> NoRef]

Requests ==
    { [id |-> 1,
       file_a |-> [kind |-> "qualified", project |-> "alpha", relative_path |-> "src/a.rs", path |-> ""],
       file_b |-> [kind |-> "qualified", project |-> "alpha", relative_path |-> "src/b.rs", path |-> ""]],
      [id |-> 2,
       file_a |-> [kind |-> "qualified", project |-> "duplicate", relative_path |-> "src/lib.rs", path |-> ""],
       file_b |-> [kind |-> "qualified", project |-> "alpha", relative_path |-> "src/b.rs", path |-> ""]],
      [id |-> 3,
       file_a |-> [kind |-> "absolute", project |-> "", relative_path |-> "", path |-> "/ws/alpha/src/a.rs"],
       file_b |-> [kind |-> "qualified", project |-> "alpha", relative_path |-> "src/b.rs", path |-> ""]] }

RequestIds == {r.id : r \in Requests}
FileIds == {f.id : f \in Files}
ChunkIds == {c.id : c \in Chunks}
Outcomes == {"ok", "rejected"}

MatchingProjects(name) == {p \in Projects : p.name = name}

ResolveFile(ref) ==
    IF ref.kind = "absolute" THEN
        LET matches == {f \in Files : f.path = ref.path} IN
        IF Cardinality(matches) = 1 THEN (CHOOSE f \in matches : TRUE).id ELSE 0
    ELSE
        LET projects == MatchingProjects(ref.project) IN
        IF Cardinality(projects) = 1 THEN
            LET pid == (CHOOSE p \in projects : TRUE).id IN
            LET matches == {f \in Files : f.project_id = pid /\ f.relative_path = ref.relative_path} IN
            IF Cardinality(matches) = 1 THEN (CHOOSE f \in matches : TRUE).id ELSE 0
        ELSE 0

ChunkFile(chunk_id) == (CHOOSE c \in Chunks : c.id = chunk_id).file_id

CandidatePairs(file_a, file_b) ==
    {[chunk_a |-> ca.id, chunk_b |-> cb.id, similarity |-> 1] :
        ca \in {c \in Chunks : c.file_id = file_a},
        cb \in {c \in Chunks : c.file_id = file_b}}

RequestFor(id) == CHOOSE r \in Requests : r.id = id

VARIABLES phase, req, responses, seen

vars == <<phase, req, responses, seen>>

PairRecord ==
    [ chunk_a: ChunkIds,
      chunk_b: ChunkIds,
      similarity: 0..1 ]

ResponseRecord ==
    [ request_id: RequestIds,
      outcome: Outcomes,
      file_a_id: FileIds \cup {0},
      file_b_id: FileIds \cup {0},
      rows: SUBSET PairRecord ]

Init ==
    /\ phase = "idle"
    /\ req = NoReq
    /\ responses = <<>>
    /\ seen = {}

PickRequest(r) ==
    /\ phase = "idle"
    /\ r \in Requests
    /\ r.id \notin seen
    /\ req' = r
    /\ phase' = "pending"
    /\ UNCHANGED <<responses, seen>>

RejectUnresolved ==
    /\ phase = "pending"
    /\ LET fa == ResolveFile(req.file_a) IN
       LET fb == ResolveFile(req.file_b) IN
       /\ (fa = 0 \/ fb = 0)
       /\ responses' =
            Append(responses,
                [ request_id |-> req.id,
                  outcome |-> "rejected",
                  file_a_id |-> fa,
                  file_b_id |-> fb,
                  rows |-> {} ])
    /\ seen' = seen \cup {req.id}
    /\ phase' = "done"
    /\ UNCHANGED req

ReturnAlignment ==
    /\ phase = "pending"
    /\ LET fa == ResolveFile(req.file_a) IN
       LET fb == ResolveFile(req.file_b) IN
       /\ fa # 0 /\ fb # 0
       /\ \E rows \in SUBSET CandidatePairs(fa, fb) :
            /\ \A p1, p2 \in rows :
                (p1 # p2) => p1.chunk_a # p2.chunk_a /\ p1.chunk_b # p2.chunk_b
            /\ responses' =
                Append(responses,
                    [ request_id |-> req.id,
                      outcome |-> "ok",
                      file_a_id |-> fa,
                      file_b_id |-> fb,
                      rows |-> rows ])
    /\ seen' = seen \cup {req.id}
    /\ phase' = "done"
    /\ UNCHANGED req

Reset ==
    /\ phase = "done"
    /\ req' = NoReq
    /\ phase' = "idle"
    /\ UNCHANGED <<responses, seen>>

Next ==
    \/ \E r \in Requests : PickRequest(r)
    \/ RejectUnresolved
    \/ ReturnAlignment
    \/ Reset

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ phase \in {"idle", "pending", "done"}
    /\ req \in Requests \cup {NoReq}
    /\ responses \in Seq(ResponseRecord)
    /\ seen \subseteq RequestIds

AmbiguousProjectReferencesRejected ==
    \A i \in 1..Len(responses) :
        LET r == RequestFor(responses[i].request_id) IN
        (Cardinality(MatchingProjects(r.file_a.project)) > 1 \/ Cardinality(MatchingProjects(r.file_b.project)) > 1) =>
            /\ responses[i].outcome = "rejected"
            /\ responses[i].rows = {}

RowsBelongToResolvedFiles ==
    \A i \in 1..Len(responses) :
        \A row \in responses[i].rows :
            /\ ChunkFile(row.chunk_a) = responses[i].file_a_id
            /\ ChunkFile(row.chunk_b) = responses[i].file_b_id

OneToOneAlignment ==
    \A i \in 1..Len(responses) :
        \A p1, p2 \in responses[i].rows :
            (p1 # p2) => p1.chunk_a # p2.chunk_a /\ p1.chunk_b # p2.chunk_b

ResolvedOrRejected ==
    \A i \in 1..Len(responses) :
        responses[i].outcome = "ok" =>
            /\ responses[i].file_a_id # 0
            /\ responses[i].file_b_id # 0

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        AmbiguousProjectReferencesRejected /\
        RowsBelongToResolvedFiles /\
        OneToOneAlignment /\
        ResolvedOrRejected)

=============================================================================
