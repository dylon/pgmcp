# `idx_cge_unique` Violation on File Deletion — Scientific Ledger

**Date opened:** 2026-05-27
**Host:** NVIDIA RTX 4060 Ti (8 GiB VRAM, Ada Lovelace, CC 8.9), Arch Linux
**Branch:** `feat/work-item-tracker` (base commit `76afa94`)
**Trigger:** operator observed a recurring `ERROR` in the live daemon log:

```json
{"timestamp":"2026-05-27T18:35:36.878174Z","level":"ERROR",
 "fields":{"message":"Failed to delete file from index",
   "path":"/home/dylon/.claude/sessions/3670555.json","worker_id":0,
   "error":"error returned from database: duplicate key value violates unique constraint \"idx_cge_unique\""},
 "target":"pgmcp::embed::pool","threadId":"ThreadId(72)"}
```

Every hypothesis, the evidence, and the fix are recorded here per the CLAUDE.md
"scientific ledger" rule.

---

## 1. Anomaly

A **plain `DELETE`** raising a **unique-constraint** violation. The delete path
is trivial — `queries::delete_file` (`src/db/queries.rs:670`) runs only:

```sql
DELETE FROM indexed_files WHERE path = $1
```

A `DELETE FROM indexed_files` cannot *directly* violate a unique index on a
different table (`code_graph_edges`). Therefore the violation must come from a
**referential-action side-effect** of the delete, not from any INSERT.

## 2. Hypotheses considered

| # | Hypothesis | Verdict |
|---|------------|---------|
| H1 | Race: the `graph-analysis` cron INSERTs an edge concurrently with the delete. | **Rejected.** The cron's INSERTs (`src/cron/graph_analysis.rs`, `src/db/queries.rs`) all use `ON CONFLICT … DO UPDATE/NOTHING` against `idx_cge_unique` and pre-dedupe their batches; an in-batch INSERT dup would also surface under `pgmcp::cron::graph_analysis`, not `embed::pool`'s delete path. |
| H2 | Test-fixture INSERTs missing `ON CONFLICT` (`pgmcp-testing/.../synthetic_corpus.rs`). | **Rejected.** Those live in the `pgmcp-testing` crate, never in the daemon that emitted this error. |
| **H3** | **`target_file_id ON DELETE SET NULL` + the COALESCE-collapsing `idx_cge_unique` collide during the delete cascade.** | **Confirmed (see §3).** |

## 3. Root cause (H3, confirmed from the DDL)

`code_graph_edges` (`src/db/migrations.rs:977`) has two FKs to `indexed_files`
with *different* delete actions, and a unique index that collapses NULL:

```sql
source_file_id BIGINT NOT NULL REFERENCES indexed_files(id) ON DELETE CASCADE
target_file_id BIGINT          REFERENCES indexed_files(id) ON DELETE SET NULL  -- ← bug

CREATE UNIQUE INDEX idx_cge_unique ON code_graph_edges(
    source_file_id, COALESCE(target_file_id, -1::BIGINT),
    edge_type, COALESCE(target_raw, ''));
```

When `indexed_files` row **X** is deleted:

- edges with `source_file_id = X` → **CASCADE-deleted** (correct);
- edges with `target_file_id = X` → `target_file_id` is **SET NULL** — *the row
  survives*. After `COALESCE`, its index key becomes
  `(source, -1, edge_type, COALESCE(target_raw, ''))`.

If another edge from the same `source` already has `target_file_id IS NULL`
with the same `(edge_type, target_raw)` — which **accumulates** every time a
previously-referenced file is deleted and its edge gets nulled — the `SET NULL`
update **collides** with `idx_cge_unique`. Postgres reports the violation as the
error of the *triggering* `DELETE`, which `?`-propagates out of `delete_file`
into `embed::pool`'s worker as "Failed to delete file from index".

This is **deterministic**, not a race: it fires whenever a delete would null a
target into a key already occupied by an earlier-nulled edge. The path
`~/.claude/sessions/*.json` matches because session files rotate constantly and
`.claude/` is densely semantic-linked, so NULL-target orphans pile up there
fastest.

### Concrete trace

`semantic` edges insert `target_raw = NULL`, so two edges from source `A`,
`A→B` and `A→C`, have keys `(A,B,'semantic','')` and `(A,C,'semantic','')`.
Delete `B`: `A→B` nulls to `(A,-1,'semantic','')` and survives. Delete `C`:
`A→C` tries to null to `(A,-1,'semantic','')` → **duplicate key** → the delete
of `C` fails.

### Scope sweep — exactly one footgun

The footgun requires a column that is *both* an `ON DELETE SET NULL` FK *and* a
member of a unique index that maps NULL to a concrete value (`COALESCE`) or uses
`NULLS NOT DISTINCT`. Auditing `src/db/migrations.rs`:

- `idx_cge_unique` (`code_graph_edges`) — `target_file_id` is SET NULL **and**
  COALESCE'd into the key → **the one defect.**
- `idx_sps_identity` (`software_pattern_sources`) — COALESCEs `url`, a plain
  `TEXT` column, not an FK → safe.
- `symbol_references` — `target_file_id`/`source_symbol_id`/`target_symbol_id`
  are SET NULL but **not** in its `UNIQUE (source_file_id, source_line,
  target_raw, ref_kind)` → safe.
- `memory_scope_tuple_uq` (`UNIQUE NULLS NOT DISTINCT`) — its FK members
  (`session_id`, `project_id`) are `ON DELETE CASCADE`, not SET NULL → safe.
- `code_graph_edges.source_symbol_id`/`target_symbol_id` — SET NULL but not in
  any unique index → safe (`source_symbol_id` was already re-tightened to
  CASCADE for a *different* reason — a CHECK conflict — at
  `migrations.rs:1309`).

## 4. Fix

Re-tighten `code_graph_edges.target_file_id` to **`ON DELETE CASCADE`** — an
edge whose target file is gone is meaningless and should be removed, exactly
like its `source_file_id` sibling. A still-valid import is rebuilt as
*unresolved* (`target_file_id NULL`, `target_raw` retained) by the
`graph-analysis` cron's `ON CONFLICT … DO UPDATE` on its next pass, so nothing
is permanently lost. (Rejected alternative: dropping `COALESCE` from the index
to make NULLs distinct — it would force rewriting every `ON CONFLICT (…
COALESCE(target_file_id,-1) …)` site, change dedupe semantics, and leave
duplicate orphan rows.)

Changes (`src/db/migrations.rs` + new submodule):

1. CREATE TABLE DDL (`migrations.rs:983`): FK now `ON DELETE CASCADE` so fresh
   installs are correct at the source.
2. Idempotent re-tighten `DO $$ … $$` block, identical idiom to the
   `source_symbol_id` re-tighten directly above it: dynamically look up the FK
   name + `confdeltype` from `pg_constraint`, rewrite only when not already
   CASCADE (`'c'`). This repairs **existing** installs, where the table already
   exists and `CREATE TABLE IF NOT EXISTS` won't alter it. No-op on re-run.
3. Migration step 7 (`src/db/migrations/v7_cge_orphan_cleanup.rs`,
   version-gated, exactly once): one-time
   `DELETE FROM code_graph_edges WHERE target_file_id IS NULL AND target_raw IS NULL`
   to clear the meaningless semantic/co-change orphans the old SET NULL already
   left behind. Import orphans (`target_raw` NOT NULL = genuine unresolved
   imports) are preserved. The deleted count is logged.

Because the fix is at the schema (FK) level, it covers **all** delete paths
automatically — `delete_file`, `delete_files_batch`, and project-cascade
deletes.

## 5. Verification

1. **Regression test** (`pgmcp-testing/tests/cge_target_file_cascade.rs`,
   live-DB-gated): seed files A, B, C; semantic edges A→B and A→C; `delete_file(B)`
   then `delete_file(C)`. Pre-fix the second delete fails with `idx_cge_unique`;
   post-fix both succeed and `count(edges WHERE source_file_id = A) = 0`. Also
   asserts the FK `confdeltype = 'c'`.
2. **Idempotency**: `migrations_post_cutover_idempotent.rs` (rerun must not
   error) and `migrations_versioning.rs` (no new version rows on rerun) cover
   the migration. The latter's stale `count == 1` assertion was corrected to a
   stability check as part of this work — with v2..v7 each recording a version
   row, `count == 1` is false against any migrated DB; it survived only because
   it had not been exercised against a populated `pgmcp_schema_versions` (e.g.
   wherever the CREATEDB-gated harness self-skips).
3. **Live**: rebuild + daemon restart (migrations run at startup) → confirm
   `SELECT confdeltype FROM pg_constraint WHERE conrelid='code_graph_edges'::regclass
   AND contype='f'` shows `'c'` for `target_file_id`, and the
   "Failed to delete file from index … idx_cge_unique" error no longer recurs
   on `~/.claude/sessions/*.json` rotation.
4. **Gate**: `./scripts/verify.sh` (build + clippy + test + smoke + formal).

### Result (2026-05-27)

`./scripts/verify.sh` passed **every gate**: build, clippy `-D warnings`, the
release test suite (1277 binary-crate unit tests + the `pgmcp-testing`
integration suites, 0 failed), the `gpu_smoke` example, and the TLA+
(`SimilarityScanFkDrift`, `CronStateMachine`) and Coq (`TransducerMandateDedup`)
formal gates. The regression test
`deleting_a_referenced_file_does_not_violate_idx_cge_unique` **ran green against
a live per-test database** (the harness has a CREATEDB-capable test role — it is
not skipped here), directly confirming the cascade fix; `migrations_versioning`
also ran with the corrected stability assertion.

## 6. Status

**Implemented and verify.sh-green on `feat/work-item-tracker` (2026-05-27).**
Changes are staged in the working tree, **not yet committed** (awaiting the
user's review per their commit-only-when-asked rule). They deploy on the next
commit + daemon rebuild + restart; the re-tighten and step-7 cleanup are
idempotent / exactly-once, so no manual DB surgery is needed.
