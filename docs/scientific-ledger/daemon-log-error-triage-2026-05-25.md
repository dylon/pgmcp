# Daemon Log-Error Triage — Scientific Ledger

**Date opened:** 2026-05-25
**Host:** NVIDIA RTX 4060 Ti (8 GiB VRAM, Ada Lovelace, CC 8.9), Arch Linux
**Log:** `~/.local/share/pgmcp/pgmcp.log`
**Trigger:** operator observed recurring `ERROR` lines in the live daemon log, e.g.

```json
{"level":"ERROR","fields":{"message":"Symbol extraction failed for project",
 "project":"MeTTa-Compiler-PR-63",
 "error":"error returned from database: invalid reference to FROM-clause entry for table \"sr\""},
 "target":"pgmcp::cron::symbol_extraction"}
```

Every hypothesis, experiment, and measurement here is reproducible from the
recorded commands, per the CLAUDE.md "scientific ledger" rule.

---

## 1. Method

Tallied all `ERROR`/`WARN` lines by `(target, message)` and captured a sample
`error` string + **last-occurrence timestamp** for each, to separate *live*
failures from *stale* ones already fixed by earlier commits in the running
binary:

```sh
python3 - "$LOG" <<'PY'   # parse JSON lines, group by (level,target,message)
...                       # (full script in the session transcript)
PY
for pat in 'FROM-clause entry for table' 'line_start does not exist' \
           'committed_at does not exist' 'CUDA_ERROR_OUT_OF_MEMORY' 'disconnected channel'; do
  printf '[%5s] last=%s :: %s\n' "$(grep -cF "$pat" "$LOG")" \
    "$(grep -F "$pat" "$LOG" | tail -1 | grep -oE '"timestamp":"[^"]*"')" "$pat"
done
```

Daemon binary mtime: `2026-05-25T21:39:46`. Newest log line: `…T03:49Z`
(≈23:49 local). Errors whose last occurrence predates the 21:39 rebuild were
emitted by the *previous* daemon process and are resolved in the running one.

## 2. Triage result

| Error | Count | Last seen (UTC) | Status |
|-------|------:|-----------------|--------|
| `invalid reference to FROM-clause entry for table "sr"` (symbol_extraction) | 12 | 2026-05-26T03:44 (≈5 min before triage) | **LIVE → fixed (this ledger)** |
| `column fs.line_start does not exist` (fuzzy-sync) | 11 | 2026-05-25T20:05 (≈8 h stale) | resolved — `src/fuzzy/sync.rs` already selects `fs.start_line` (HEAD `1f5703c`) |
| `column gc.committed_at does not exist` (drift tools) | 2 | 2026-05-25T04:51 (≈23 h stale) | resolved — commit `ece05fa` "rename gc.committed_at → gc.author_date across 8 tools" |
| `CUDA_ERROR_OUT_OF_MEMORY` (embed / migration) | 356 | 2026-05-25T20:26 (≈7 h stale) | resolved — HEAD `1f5703c` "Bound resident GPU embedders" |
| `sending on a disconnected channel` / `Failed to submit IndexFile` | ~186 | 2026-05-25T04–16 (≈12–24 h stale) | shutdown-cycle artifacts; not recurring on the running process |

Non-error noise (benign, no action): `slow statement` WARN (perf alert
threshold), `not valid UTF-8 (skipping)` (binary files), `pandoc exited with
status 64` on a single malformed `…/arxiv/pstricks.tex`, and
`statement timeout` on very large projects' symbol-resolution pass (separate
perf concern, see §5).

**Conclusion: exactly one live bug — the `sr` FROM-clause error.**

## 3. Root cause (the `sr` bug)

`src/db/queries.rs`, the per-project symbol-reference resolver
(`resolve_project_symbol_references`), runs four tiered `UPDATE
symbol_references sr …` phases on one transaction. Phase 3 (`bare_name_*`,
added in commit `8068a5bf`, 2026-05-25 16:43, an ancestor of HEAD) used a CTE
and joined it to the UPDATE target inside a `JOIN … ON`:

```sql
UPDATE symbol_references sr
SET …
FROM file_symbols fs
JOIN indexed_files tgt_f ON tgt_f.id = fs.file_id
JOIN cand ON cand.ref_id = sr.id      -- ← references UPDATE target `sr` in a JOIN ON
WHERE …
```

In Postgres an `UPDATE … FROM` target alias is in scope only for
`SET`/`WHERE`/`RETURNING`, **not** for `JOIN … ON` predicates between
`FROM`-list members. Referencing `sr` there raises *"invalid reference to
FROM-clause entry for table sr"*, which `?`-propagates out of
`extract_project_symbols` and aborts the whole symbol-extraction run for the
project. Phase 2 (`exact_via_import`) had hit this exact trap earlier and was
fixed by moving its `e.source_file_id = sr.source_file_id` correlation into
`WHERE` (see the comment at the phase-2 query); phase 3 reintroduced the
pattern with `cand`.

## 4. Fix

Mirror the phase-2 fix: make `cand` a comma `FROM`-list member and move the
`cand.ref_id = sr.id` correlation into `WHERE`.

```sql
FROM file_symbols fs
JOIN indexed_files tgt_f ON tgt_f.id = fs.file_id,
     cand
WHERE tgt_f.project_id = $1
  AND cand.ref_id = sr.id      -- correlate to the UPDATE target in WHERE
  AND sr.target_raw = fs.name
  AND sr.resolution_kind IS NULL
```

Semantically identical (inner join on `cand.ref_id = sr.id`); only the legal
placement changes.

## 5. Verification

`EXPLAIN` (plans without executing — non-destructive) against the live DB,
old vs. new phase-3 SQL:

```
OLD: ERROR:  invalid reference to FROM-clause entry for table "sr"   (reproduced)
NEW: Update on symbol_references sr  (cost=48.13..7424.40 rows=0 width=0)   (valid plan)
```

Hypothesis confirmed by reproduction; fix confirmed by a clean query plan. The
fix is committed-code (not the concurrent A2A work, whose `queries.rs` changes
are in `memory_retention_purge`) and deploys on the next daemon
rebuild + restart.

## 6. Follow-on (not a bug)

`statement timeout` on `symbol_extraction` for very large projects: the
resolution pass already raises `SET LOCAL statement_timeout = '300s'`; a few
huge projects still exceed it. With the `sr` abort removed, affected projects
will at least complete phases 1–2/4; if timeouts persist on the resolution
pass, batch the phase-3 CTE by source file rather than whole-project. Tracked
here, not yet observed as needed post-fix.
