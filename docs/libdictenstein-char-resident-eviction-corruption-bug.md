# libdictenstein bug: resident-budget char eviction corrupts the sequential-sibling on-disk layout

**Status:** open — to be fixed in `libdictenstein` by the libdictenstein agent.
**Reporter:** pgmcp agent (found while wiring pgmcp's `fuzzy-sync` OOM fix).
**Repo:** `/home/dylon/Workspace/f1r3fly.io/libdictenstein`
**Date:** 2026-07-08

---

## One-line summary

Enabling the resident-heap budget on a **char** `PersistentARTrieChar`
(`EvictionConfig::resident_budget_bytes = Some(_)`) and calling `checkpoint()`
**incrementally during a bulk load** corrupts the on-disk trie: the very next
checkpoint fails with

```
Corrupted file: char v2 sequential child mismatch at index 0:
got ArenaSlot { arena_id: 0, slot_id: 148 }, expected arena 1 slot 148
```

The corruption is produced on a **freshly created** trie (no pre-existing bad
state), so it is introduced by the eviction+checkpoint interaction itself.

## Symptom / exact error

Raised in `src/persistent_artrie/char/serialization_char.rs:1165`
(`PersistentARTrieError::corrupted`, "char v2 sequential child mismatch …")
during a `checkpoint()` that runs after a resident-budget eviction pass.

## Trigger conditions (all required)

1. A **char** trie (`PersistentARTrieChar` / pgmcp's `FuzzyIndex<V>`).
2. Eviction enabled with `resident_budget_bytes: Some(b)` for a **small** `b`
   (so the post-checkpoint budget tail actually fires). With `None`
   (the default) the bug does **not** occur — the budget tail is skipped.
3. `checkpoint()` called **repeatedly while still inserting** (incremental
   checkpointing), so an eviction pass runs *mid-build*, between inserts, and a
   later checkpoint re-serializes a node whose children were just scattered.

A single terminal `checkpoint()` after all inserts (the pre-existing usage) does
not appear to trip it, because there is no *subsequent* checkpoint to re-serialize
the post-eviction, scattered layout.

## Root cause (hypothesis, strongly supported by the code)

The **sequential-sibling serialization optimization** and the **individual-node
eviction** hold contradictory assumptions about arena-slot layout:

- **Producer of the invariant** — `serialization_char.rs:191‑192`: when a node's
  children occupy *contiguous* arena slots, the node is encoded as
  `(first_child_slot, count)` instead of N explicit child pointers
  (`uses_sequential_siblings()`). The reader/validator (`serialization_char.rs:1080‑1165`)
  then *requires* every child `idx` to satisfy
  `slot.arena_id == first_child.arena_id && slot.slot_id == first_child.slot_id + idx`.

- **Violator of the invariant** — `char/mod.rs:1351 evict_overlay_nodes`
  (driven by `coordinator.rs:417 force_eviction_char_resident`, invoked from the
  checkpoint tail at `char/persist.rs:630‑641`): it evicts **individual** overlay
  nodes **leaf-first by coldness/LRU**, one at a time via
  `evict_overlay_node_at_path`, assigning each a disk `ArenaSlot` independently.
  It does **not** keep a parent's children contiguous or in one arena.

So after a partial eviction pass, a `use_sequential` parent's children end up
across **different arenas** and/or **non-contiguous slots**, while the parent's
serialized metadata still claims the sequential `(first_child_slot, count)`
encoding. The next `checkpoint()` re-serializes that parent, walks its children,
and the contiguity check fails at the first child whose actual slot ≠
`first_child.slot_id + idx` / whose arena ≠ `first_child.arena_id`.

The concrete error (`got arena 0 slot 148, expected arena 1 slot 148` at idx 0)
matches exactly: the first child was relocated into a *different arena* (0) than
the parent's `first_child_slot` (arena 1) assumes.

## Minimal reproduction

Concrete, currently-failing repro (pgmcp side, via the `FuzzyIndex<V>` wrapper —
runnable today):

- Test: `resident_budget_eviction_reclaims_and_preserves_terms`
  in `pgmcp/src/fuzzy/persistent_artrie.rs`.
- It: opens a fresh `FuzzyIndex::<i64>`; `enable_eviction` with
  `resident_budget_bytes: Some(64*1024)`, `enable_memory_pressure_monitor: false`;
  inserts 20 000 terms `symbol_{i:08}`; calls `checkpoint()` every 2 000 inserts.
  → panics at the first post-eviction `checkpoint()` with the mismatch error.

Equivalent **libdictenstein-native** repro (for the fix repo — adjust to the real
`PersistentARTrieChar` API):

```rust
#[test]
fn resident_budget_char_eviction_preserves_sequential_layout() {
    use crate::persistent_artrie::eviction::EvictionConfig;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("evict.artrie");
    let trie = PersistentARTrieChar::<i64>::open_or_create(&path).unwrap();
    trie.enable_eviction(EvictionConfig {
        resident_budget_bytes: Some(64 * 1024),
        enable_memory_pressure_monitor: false,
        ..EvictionConfig::default()
    }).unwrap();
    for i in 0..20_000i64 {
        trie.insert_with_value(&format!("symbol_{i:08}"), i).unwrap();
        if i % 2_000 == 0 {
            trie.checkpoint().unwrap(); // <- corrupts: "char v2 sequential child mismatch"
        }
    }
    trie.checkpoint().unwrap();
    // Post-fix expectations:
    //   * no corruption error,
    //   * eviction actually reclaimed nodes (nodes_evicted > 0),
    //   * every term still queryable after drop + reopen (completeness).
}
```

There is (as of this writing) **no** libdictenstein test exercising
`resident_budget_bytes = Some(_)` + incremental checkpoint on a **char** trie —
only byte-eviction tests (`overlay_eviction_byte_tests.rs`) and the driver
correspondence test. This path is effectively untested for char tries, consistent
with the bug surviving the 2026-06 lock-free overlay refactor.

## Suggested fix directions (libdictenstein agent to decide)

Any one of:

1. **Downgrade on scatter (preferred, minimal):** when eviction relocates a child
   such that a parent can no longer satisfy the sequential-sibling contiguity
   invariant, drop that parent's `uses_sequential_siblings` flag and serialize N
   explicit child pointers. Correct for arbitrary post-eviction layouts.
2. **Recompute `use_sequential` at serialize time from ACTUAL child slots** rather
   than trusting stale metadata — only emit the sequential encoding when the
   children are in fact contiguous in one arena; otherwise emit explicit pointers.
3. **Contiguity-preserving eviction:** evict a `use_sequential` parent's children
   as a group into contiguous slots (more invasive; fights the LRU/leaf-first
   ordering, so likely worse than 1/2).

Whichever is chosen, please add the char resident-budget eviction+reopen test
above (asserting no corruption, `nodes_evicted > 0`, and full completeness after
reopen) so the path is covered going forward.

## Impact + coordination

This blocks pgmcp's `fuzzy-sync` OOM fix. pgmcp's plan is to bound `fuzzy-sync`
rebuild RAM by setting `resident_budget_bytes` and checkpointing incrementally
(so the 11.5 GB `default`-project trie builds in ~1 GB instead of OOM-killing the
daemon). That fix is **inert/unsafe until this corruption is resolved**; pgmcp is
currently mitigated only by a cgroup `MemoryMax` backstop + a temporarily-raised
`fuzzy_sync_interval`. Once fixed in libdictenstein, pgmcp flips
`[fuzzy] resident_budget_bytes` on and validates via the live RSS-plateau test.

pgmcp-side code already staged against this contract (not yet deployed):
`pgmcp/src/config.rs` (`resident_budget_bytes`, `checkpoint_every_rows`,
`eviction_config()`), `pgmcp/src/fuzzy/sync.rs` (keyset pagination + per-page
checkpoint), `pgmcp/src/cron/fuzzy_sync.rs` (`prime_eviction` before rebuild).
