-------------------------- MODULE AtomicFileReplacement --------------------------
(***************************************************************************)
(* Active indexer all-or-nothing replacement for one indexed file.         *)
(*                                                                         *)
(* Models `src/db/queries/chunks.rs::replace_indexed_file`, which updates  *)
(* indexed_files, deletes old file_chunks, inserts replacement chunks, and  *)
(* finalizes content_hash inside one SQL transaction. The important safety  *)
(* contract is not performance; it is that readers never observe the        *)
(* in-transaction NULL hash or a partially replaced chunk set. Any lock     *)
(* timeout, insert failure, or finalize failure aborts back to the old      *)
(* complete state.                                                          *)
(***************************************************************************)

EXTENDS Naturals, TLC

OldHash == "old-hash"
NewHash == "new-hash"
NullHash == "null"
OldChunks == {"old"}
NewChunks == {"new"}

VARIABLES
    phase,         \* "idle" | "tx_meta" | "tx_deleted" | "tx_inserted" | "tx_finalized" | "committed" | "aborted"
    visible_hash,  \* hash visible outside the transaction
    visible_chunks,\* chunks visible outside the transaction
    base_hash,     \* visible hash captured at transaction start
    base_chunks,   \* visible chunks captured at transaction start
    tx_hash,       \* transaction-local hash
    tx_chunks      \* transaction-local chunks

vars == <<phase, visible_hash, visible_chunks, base_hash, base_chunks, tx_hash, tx_chunks>>

Init ==
    /\ phase = "idle"
    /\ visible_hash = OldHash
    /\ visible_chunks = OldChunks
    /\ base_hash = OldHash
    /\ base_chunks = OldChunks
    /\ tx_hash = OldHash
    /\ tx_chunks = OldChunks

BeginReplacement ==
    /\ phase = "idle"
    /\ phase' = "tx_meta"
    /\ base_hash' = visible_hash
    /\ base_chunks' = visible_chunks
    /\ tx_hash' = NullHash
    /\ tx_chunks' = visible_chunks
    /\ UNCHANGED <<visible_hash, visible_chunks>>

DeleteChunks ==
    /\ phase = "tx_meta"
    /\ phase' = "tx_deleted"
    /\ tx_chunks' = {}
    /\ UNCHANGED <<visible_hash, visible_chunks, base_hash, base_chunks, tx_hash>>

InsertChunks ==
    /\ phase = "tx_deleted"
    /\ phase' = "tx_inserted"
    /\ tx_chunks' = NewChunks
    /\ UNCHANGED <<visible_hash, visible_chunks, base_hash, base_chunks, tx_hash>>

FinalizeHash ==
    /\ phase = "tx_inserted"
    /\ phase' = "tx_finalized"
    /\ tx_hash' = NewHash
    /\ UNCHANGED <<visible_hash, visible_chunks, base_hash, base_chunks, tx_chunks>>

Commit ==
    /\ phase = "tx_finalized"
    /\ phase' = "committed"
    /\ visible_hash' = tx_hash
    /\ visible_chunks' = tx_chunks
    /\ UNCHANGED <<base_hash, base_chunks, tx_hash, tx_chunks>>

Abort ==
    /\ phase \in {"tx_meta", "tx_deleted", "tx_inserted", "tx_finalized"}
    /\ phase' = "aborted"
    /\ tx_hash' = base_hash
    /\ tx_chunks' = base_chunks
    /\ UNCHANGED <<visible_hash, visible_chunks, base_hash, base_chunks>>

Reset ==
    /\ phase \in {"committed", "aborted"}
    /\ phase' = "idle"
    /\ base_hash' = visible_hash
    /\ base_chunks' = visible_chunks
    /\ tx_hash' = visible_hash
    /\ tx_chunks' = visible_chunks
    /\ UNCHANGED <<visible_hash, visible_chunks>>

Next ==
    \/ BeginReplacement
    \/ DeleteChunks
    \/ InsertChunks
    \/ FinalizeHash
    \/ Commit
    \/ Abort
    \/ Reset

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ phase \in {"idle", "tx_meta", "tx_deleted", "tx_inserted", "tx_finalized", "committed", "aborted"}
    /\ visible_hash \in {OldHash, NewHash}
    /\ visible_chunks \in {OldChunks, NewChunks}
    /\ base_hash \in {OldHash, NewHash}
    /\ base_chunks \in {OldChunks, NewChunks}
    /\ tx_hash \in {OldHash, NewHash, NullHash}
    /\ tx_chunks \in {OldChunks, NewChunks, {}}

VisibleNeverPartial ==
    \/ /\ visible_hash = OldHash
       /\ visible_chunks = OldChunks
    \/ /\ visible_hash = NewHash
       /\ visible_chunks = NewChunks

AbortRollsBack ==
    phase = "aborted" =>
        /\ visible_hash = base_hash
        /\ visible_chunks = base_chunks

CommitPublishesCompleteReplacement ==
    phase = "committed" =>
        /\ visible_hash = NewHash
        /\ visible_chunks = NewChunks

NullHashNeverVisible ==
    visible_hash # NullHash

THEOREM SpecImpliesAtomicity ==
    Spec => [](
        TypeOK /\
        VisibleNeverPartial /\
        AbortRollsBack /\
        CommitPublishesCompleteReplacement /\
        NullHashNeverVisible)

=============================================================================
