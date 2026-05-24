---- MODULE SimilarityScanFkDrift ----
(*
 * Long-running cron's tolerance to file_chunks deletion mid-pass.
 *
 * Models the pattern documented in
 * `feedback_long_running_jobs_must_handle_fk_drift.md`: a
 * similarity-scan cron caches a set of chunk_ids at the start of
 * its pass, then processes them in batches over many minutes. If
 * the indexer deletes chunks (via `delete_files_batch` →
 * ON DELETE CASCADE) mid-pass, the cron's cached chunk_ids may
 * include rows that no longer exist.
 *
 * Invariant we need to hold:
 *   NoOrphanFkInsert — the cron MUST NOT INSERT into
 *     cross_project_similarities with a chunk_id that does not
 *     exist in file_chunks. The fix in code is the `WHERE EXISTS`
 *     guard on the bulk INSERT; this spec models that guard.
 *
 * Plan reference:
 *   ~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md
 *   Phase 12.
 *)
EXTENDS Naturals, FiniteSets

CONSTANT ChunkIds

VARIABLES
    file_chunks,           \* set of chunk_ids currently in the DB
    cron_cache,            \* set of chunk_ids the cron snapshotted
    similarities           \* set of inserted similarity rows (chunk_id_a, chunk_id_b)

vars == <<file_chunks, cron_cache, similarities>>

TypeOK ==
    /\ file_chunks  \subseteq ChunkIds
    /\ cron_cache   \subseteq ChunkIds
    /\ similarities \subseteq (ChunkIds \X ChunkIds)

Init ==
    /\ file_chunks  = ChunkIds
    /\ cron_cache   = {}
    /\ similarities = {}

(* The cron starts a pass: snapshots the current file_chunks set.  *)
StartScan ==
    /\ cron_cache = {}
    /\ cron_cache' = file_chunks
    /\ UNCHANGED <<file_chunks, similarities>>

(* The indexer deletes a chunk during the scan. file_chunks shrinks
 * but cron_cache still holds the deleted id. Postgres's
 * ON DELETE CASCADE removes any similarities involving the deleted
 * chunk, which is what keeps `NoOrphanFkInsert` an inductive
 * invariant — without the cascade the inserted pair would dangle.  *)
DeleteChunk(c) ==
    /\ c \in file_chunks
    /\ file_chunks' = file_chunks \ {c}
    /\ similarities' = { pair \in similarities :
                            pair[1] # c /\ pair[2] # c }
    /\ UNCHANGED cron_cache

(* The cron inserts a similarity pair, guarded by WHERE EXISTS so
 * stale ids are filtered out at INSERT time. This is the
 * load-bearing invariant — without the guard, the INSERT would
 * succeed against a non-existent FK target and Postgres would
 * raise SQLSTATE 23503.                                           *)
InsertPair(a, b) ==
    /\ a \in cron_cache /\ b \in cron_cache
    /\ a # b
    /\ a \in file_chunks      \* WHERE EXISTS guard
    /\ b \in file_chunks
    /\ similarities' = similarities \cup {<<a, b>>}
    /\ UNCHANGED <<file_chunks, cron_cache>>

(* The cron finishes: clears its cache.                            *)
FinishScan ==
    /\ cron_cache # {}
    /\ cron_cache' = {}
    /\ UNCHANGED <<file_chunks, similarities>>

Next ==
    \/ StartScan
    \/ \E c \in ChunkIds : DeleteChunk(c)
    \/ \E a, b \in ChunkIds : InsertPair(a, b)
    \/ FinishScan

Spec == Init /\ [][Next]_vars

(* === Safety invariants === *)

NoOrphanFkInsert ==
    \A pair \in similarities :
        pair[1] \in file_chunks /\ pair[2] \in file_chunks

CacheNeverExceedsKnownIds ==
    cron_cache \subseteq ChunkIds

Invariants ==
    /\ TypeOK
    /\ NoOrphanFkInsert
    /\ CacheNeverExceedsKnownIds

====
