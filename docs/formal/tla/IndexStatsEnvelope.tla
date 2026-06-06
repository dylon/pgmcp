------------------------------- MODULE IndexStatsEnvelope -------------------------------
(***************************************************************************)
(* `index_stats` response envelope.                                        *)
(*                                                                         *)
(* The tool increments the live MCP request counter, snapshots the          *)
(* StatsTracker, reads DB index counts through DbClient, and optionally     *)
(* enriches with workspace-wide effect counts when a raw PgPool is          *)
(* available. The snapshot must survive DB/effect query failures.           *)
(***************************************************************************)

EXTENDS Naturals, FiniteSets

DbCounts ==
    { [projects |-> 0, indexed_files |-> 0, chunks |-> 0, total_bytes |-> 0],
      [projects |-> 5, indexed_files |-> 100, chunks |-> 400, total_bytes |-> 1048576] }

Effects ==
    { [effect |-> "io", count |-> 3],
      [effect |-> "unsafe", count |-> 2] }

Requests ==
    { [id |-> 1, db_ok |-> TRUE, has_pool |-> TRUE, effects_ok |-> TRUE,
       counts |-> CHOOSE c \in DbCounts : c.indexed_files = 100,
       concurrent_mcp_requests |-> 0],
      [id |-> 2, db_ok |-> TRUE, has_pool |-> FALSE, effects_ok |-> TRUE,
       counts |-> CHOOSE c \in DbCounts : c.indexed_files = 0,
       concurrent_mcp_requests |-> 2],
      [id |-> 3, db_ok |-> FALSE, has_pool |-> TRUE, effects_ok |-> TRUE,
       counts |-> CHOOSE c \in DbCounts : c.indexed_files = 100,
       concurrent_mcp_requests |-> 0],
      [id |-> 4, db_ok |-> TRUE, has_pool |-> TRUE, effects_ok |-> FALSE,
       counts |-> CHOOSE c \in DbCounts : c.indexed_files = 100,
       concurrent_mcp_requests |-> 5] }

RequestIds == {r.id : r \in Requests}

ZeroCounts == [projects |-> 0, indexed_files |-> 0, chunks |-> 0, total_bytes |-> 0]

SnapshotFor(r) ==
    [ files_indexed |-> r.counts.indexed_files,
      chunks_embedded |-> r.counts.chunks,
      bytes_processed |-> r.counts.total_bytes,
      mcp_requests |-> 1 + r.concurrent_mcp_requests,
      mcp_errors |-> 0 ]

IndexBlockFor(r) ==
    IF r.db_ok THEN
        [ available |-> TRUE,
          error |-> "",
          projects |-> r.counts.projects,
          indexed_files |-> r.counts.indexed_files,
          chunks |-> r.counts.chunks,
          total_bytes |-> r.counts.total_bytes ]
    ELSE
        [ available |-> FALSE,
          error |-> "db error",
          projects |-> 0,
          indexed_files |-> 0,
          chunks |-> 0,
          total_bytes |-> 0 ]

EffectRowsFor(r) ==
    IF r.has_pool /\ r.effects_ok THEN Effects ELSE {}

VARIABLES req, response

vars == <<req, response>>

SnapshotRecord ==
    [ files_indexed: Nat,
      chunks_embedded: Nat,
      bytes_processed: Nat,
      mcp_requests: Nat,
      mcp_errors: Nat ]

IndexRecord ==
    [ available: BOOLEAN,
      error: {"", "db error"},
      projects: Nat,
      indexed_files: Nat,
      chunks: Nat,
      total_bytes: Nat ]

ResponseRecord ==
    [ request_id: RequestIds,
      snapshot: SnapshotRecord,
      index: IndexRecord,
      effect_breakdown: SUBSET Effects ]

Init ==
    /\ req \in Requests
    /\ response =
        [ request_id |-> req.id,
          snapshot |-> SnapshotFor(req),
          index |-> IndexBlockFor(req),
          effect_breakdown |-> EffectRowsFor(req) ]

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ req \in Requests
    /\ response \in ResponseRecord

SnapshotAlwaysPresent ==
    response.snapshot \in SnapshotRecord

LocalRequestIncrementVisible ==
    response.snapshot.mcp_requests >= 1

IndexCountsExactWhenAvailable ==
    req.db_ok =>
        /\ response.index.available
        /\ response.index.projects = req.counts.projects
        /\ response.index.indexed_files = req.counts.indexed_files
        /\ response.index.chunks = req.counts.chunks
        /\ response.index.total_bytes = req.counts.total_bytes

DbFailureOnlyDisablesIndexBlock ==
    ~req.db_ok =>
        /\ ~response.index.available
        /\ response.index.error = "db error"
        /\ response.index.projects = 0
        /\ response.index.indexed_files = 0
        /\ response.index.chunks = 0
        /\ response.index.total_bytes = 0
        /\ SnapshotAlwaysPresent

AllCountsNonNegative ==
    /\ response.snapshot.files_indexed \in Nat
    /\ response.snapshot.chunks_embedded \in Nat
    /\ response.snapshot.bytes_processed \in Nat
    /\ response.snapshot.mcp_requests \in Nat
    /\ response.index.projects \in Nat
    /\ response.index.indexed_files \in Nat
    /\ response.index.chunks \in Nat
    /\ response.index.total_bytes \in Nat
    /\ \A effect \in response.effect_breakdown : effect.count \in Nat

EffectBreakdownGraceful ==
    (~req.has_pool \/ ~req.effects_ok) => response.effect_breakdown = {}

EffectBreakdownExactWhenAvailable ==
    req.has_pool /\ req.effects_ok => response.effect_breakdown = Effects

THEOREM SpecImpliesInvariants ==
    Spec => [](
        TypeOK /\
        SnapshotAlwaysPresent /\
        LocalRequestIncrementVisible /\
        IndexCountsExactWhenAvailable /\
        DbFailureOnlyDisablesIndexBlock /\
        AllCountsNonNegative /\
        EffectBreakdownGraceful /\
        EffectBreakdownExactWhenAvailable)

=============================================================================
