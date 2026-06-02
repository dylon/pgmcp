# Symbol-coverage collapse — root-cause ledger (2026-06-02)

**Symptom that started it:** `fuzzy_symbol_search("FcmBackend", "pgmcp")` returns
nothing, and pgmcp cannot fuzzy-search its own code. Smoke-testing the search /
fuzzy features surfaced that pgmcp had symbols for only **117 / ~1000** files,
and core `src/` symbols (`make_backend`, `FcmBackend`, `rrf_score`,
`semantic_search`) were **absent from `file_symbols` entirely**.

This ledger records the investigation, the two independent root causes, the
fixes, the tests, and the verification. It is written so the work can be
reconstructed from scratch.

---

## 1. Method

Treated every "empty result" as a hypothesis to falsify against ground truth
(exact `grep`, direct `psql`), never assuming the tool was broken. Mechanisms
were proven correct on a *healthy* project before concluding the data feeding
them was at fault.

Baseline facts (live DB, project `pgmcp`, id `395629`):

```
file_symbols coverage        117 / 1004 files     (≈12%)
distinct symbol names         810
backend-language rust files   920
  └─ content IS NULL          836   (≈91%)
SEED_EFFECTS (Rust vocab)      48
effect_catalog (DB)            43   ← 5 short
```

Contrast: `f1r3node` had 1931/2225 (87%) — see RC2 §3.2 for why.

---

## 2. RC1 — `effect_catalog` drift (FK) silently skips files

### Hypothesis
A symbol carrying an effect missing from `effect_catalog` makes the
`symbol_effects_effect_fkey` FK reject the insert; the per-file transaction
aborts and the **whole file** is skipped.

### Evidence
- `symbol_effects.effect` → `effect_catalog(name)` ON DELETE RESTRICT
  (`v2_shadow_asr.rs:199`).
- `SEED_EFFECTS` (48) − `effect_catalog` (43) = **5 missing**, all from the v21
  concurrency work: `await_point`, `channel_select`, `lock_acquire`,
  `lock_release`, `thread_spawn`. `await_point` is emitted for every `.await`,
  so nearly every async file tripped it.
- Daemon log (pre-fix): `Symbol extraction failed for file (skipping) … insert
  or update on table "symbol_effects" violates foreign key constraint
  "symbol_effects_effect_fkey"` across `pgmcp`, `f1r3node-rust`, etc.
- `bulk_insert_symbol_effects` is one multi-row INSERT; `ON CONFLICT` does **not**
  absorb FK violations, so one unknown effect aborts the statement.

### Root cause
`seed_catalog` runs only inside the **version-gated** v2 migration step
(`apply_step` short-circuits via `version_applied`). Effects added to the Rust
vocabulary *after* v2 never reach an already-migrated database. The v2 docstring
claimed re-running "picks up vocabulary edits" — true only on a fresh install,
false for every existing DB. **There was no runtime invariant in either
direction** (the only catalog-vs-vocab assertion lived in a `#[cfg(test)]`
block).

### Fix (durable)
`reconcile_vocabulary_catalogs(pool)` — **unconditional, idempotent, every boot**
from `run_migrations` (after the v23 step, before `ensure_memory_unified_views`,
which sources both catalogs). It upserts `effect_catalog` from `SEED_EFFECTS` and
`type_tag_catalog` from `SEED_TYPE_TAGS` (shared `seed_catalog`, lifted out of v2),
then verifies catalog ⊇ vocabulary and logs `error!` + leaves a residual gap to
the regression test. **Non-fatal** — a stale catalog must never stop the daemon
from starting; the *hard* assertion is the test.

Chose this (every-boot reconcile) over a one-shot v24 re-seed because a one-shot
re-arms the same foot-gun for the *next* vocabulary addition. Confirmed the drift
survives a restart (DB went v22→v23 on a restart and the gap persisted), which is
the empirical case for an every-boot mechanism.

### Tests
- `vocabulary.rs::tests::concurrency_effects_present_in_seed` — no-DB tripwire:
  the 5 concurrency effects stay in `SEED_EFFECTS`.
- `pgmcp-testing/tests/vocabulary_catalog_parity.rs` — real-DB: catalog ⊇ vocab
  after migrations; **drift-and-heal** (delete the 5, re-run migrations, assert
  they return). This is the catalog-superset test ADR-003 mandated but never had.

### Live remediation already applied (2026-06-02)
Seeded the 5 rows into `effect_catalog` (now 48) so the running daemon's
extraction stopped FK-skipping immediately. A post-fix run logged **zero** FK
skips and the 5 effects now populate `symbol_effects` (`await_point=67`,
`lock_acquire=7`, `lock_release=7`, `thread_spawn=2`). The durable code fix makes
this permanent and reproducible on fresh installs.

---

## 3. RC2 — content-omission with no disk fallback (DOMINANT)

### Hypothesis
Symbol extraction only ever sees files with inline `content`, so files stored
under the asymmetric-storage policy (`content = NULL`, recoverable from disk) are
never extraction candidates — regardless of the catalog fix.

### Evidence
- `list_files_for_symbol_extraction` filtered `AND content IS NOT NULL`; Phase B
  did `None => continue`.
- The production indexer (`src/embed/pool.rs`) writes `content = NULL,
  content_recoverable_from_disk = true` for every non-document file **in the same
  upsert** — so Rust files have **no content-present window**. The
  `files_with_content_omitted` metric is literally documented "deliberately
  stored as NULL because the source is recreate-cheap from disk".
- 836/920 pgmcp Rust files had `content IS NULL`; `src/fcm/mod.rs` (home of
  `make_backend`) among them. After the RC1 fix, a re-extraction still produced
  only 84 files of symbols — exactly the inline-content minority.

### 3.2 Why f1r3node was 87% but pgmcp ≈12%
`f1r3node`'s `indexed_at` is frozen (2026-04-27 → 05-23): it was indexed under an
older full-content snapshot and never re-scanned, so its rows retain inline
content and were extractable. `pgmcp` is continuously re-scanned, so the
asymmetric policy nulls its content on every pass and extraction never saw it.
**100% of pgmcp's 836 NULL Rust files have `content_recoverable_from_disk = true`
+ a non-NULL `content_hash`** → the disk fast-path recovers every one.

### Fix (a + c)
- **Disk fallback (a):** dropped the `content IS NOT NULL` gate from Phase A/B;
  Phase B now recovers a NULL-content file's text via
  `db::disk_read::read_disk_verified(path, recoverable, content_hash)` — a shared,
  hash-verified read extracted from the `read_file` tool's fast-path so the two
  cannot drift. Hash mismatch / missing / IO-error / not-recoverable are counted
  (`symbol_extraction_disk_*`) and skipped (and hold the watermark — see F1).
- **Incremental-skip (c, v24):** new nullable `indexed_files.extracted_content_hash`
  records the `content_hash` at last successful extraction; an unchanged file is
  skipped without a re-parse. This keeps full re-scans affordable now that NULL
  files are no longer filtered out.

Rejected (b) "stop nulling backend files" — it defeats the storage optimization
for exactly the largest files and leaves the cron fragile to any future NULL.

### Tests
`pgmcp-testing/tests/symbol_extraction_disk_fallback.rs` (real-DB):
- a content-NULL file whose bytes live on disk with a matching `content_hash` is
  extracted (symbol present; `symbol_extraction_disk_reads ≥ 1`); a forced
  re-scan then incremental-skips it (`symbol_extraction_unchanged_skips ≥ 1`).
- a content-NULL file with a **wrong** hash yields no symbols, counts a
  `disk_hash_mismatches`, and leaves the watermark **before** its `modified_at`
  (F1).

---

## 4. F1 — watermark must not advance past skipped files

`extract_project_symbols` advanced the per-project watermark to `NOW()`
unconditionally, so any file skipped on error (FK, disk mismatch, parse failure)
fell *behind* the watermark and was never retried on incremental runs. Fix:
track the smallest `modified_at` among skipped files and set the watermark to
`min_skipped − 1µs` (or `NOW()` when nothing was skipped). The 1µs back-off keeps
the skipped file inside the strict `modified_at > watermark` predicate so it is
re-listed next run; successful newer files re-list too but the incremental-skip
makes them a cheap no-op. Monotonic on incremental runs.

---

## 5. F2 — per-project `trigger_cron`

`trigger_cron job="symbol-extraction"` looped **all** projects `ORDER BY id`
synchronously; the ~300s MCP tool timeout cancelled it mid-run, starving
higher-id projects. Added an optional `project` (name or numeric id) param to
`TriggerCronParams` (`#[serde(default)]` — absent ⇒ unchanged all-projects
behavior) and per-project entry points to all three heavy crons
(`run_{symbol_extraction,call_graph,function_metrics}_for_project`), wired in the
dispatch. Lets an operator scope a manual trigger to one project within the
budget.

---

## 6. Verification

- `cargo check --all-targets` — clean (zero pgmcp errors/warnings).
- `./scripts/verify.sh` — single completion gate (fmt, build --all-targets,
  clippy -D warnings, release bin tests, `pgmcp-testing` release tests, gpu
  smoke). **Result: see commit / §7.**
- Live acceptance after rebuild + daemon restart + re-extraction + `fuzzy-sync`:
  `fuzzy_symbol_search("FcmBackend","pgmcp")` returns a hit and pgmcp `src/`
  symbols are present (target: coverage ≈900+, not 117). **Result: see §7.**

---

## 7. Results log

- 2026-06-02 — RC1 live data fix applied (effect_catalog 43→48); post-fix
  extraction run had 0 FK skips. RC1+RC2+F1+F2 code landed; `cargo check
  --all-targets` clean. Freed `target/debug` (disk was 100% full → linker SIGBUS).
- 2026-06-02 — **`verify.sh`: all 8 gates passed** (1627 tests, 0 failed). One
  transient first-run failure was the concurrently-edited `libdictenstein`
  path-dependency briefly release-broken (a `cfg(debug_assertions)`-gated `V`);
  it release-compiled on re-run, untouched by us.
- 2026-06-02 — **Live acceptance: ALL criteria met.** Daemon restarted on the new
  binary → boot log `migration step applied v24 extracted_content_hash_v1` +
  `vocabulary catalogs reconciled (catalog ⊇ vocabulary verified) effects=48
  type_tags=77` (RC1 durable, live). Forced re-extraction (`trigger_cron
  symbol-extraction project=pgmcp` — F2):
  `Symbol extraction complete … files: 941, symbols: 10748, references: 120318,
  skipped_min_modified: None` (F1: zero skips).
  - Coverage **117 → 938 files** (distinct symbol names **810 → 8440**).
  - `src/fcm/mod.rs` (content NULL) **0 → 21 symbols** — RC2 disk-fallback proof.
  - Targets present in `file_symbols`: `FcmBackend`, `make_backend`, `rrf_score`,
    `semantic_search` (+ the just-written `read_disk_verified`,
    `reconcile_vocabulary_catalogs`).
  - **`fuzzy_symbol_search("FcmBakend","pgmcp") → FcmBackend` (dist 1),
    `vocabulary_size: 8441`** — the original smoke-test failure, fixed.
  - Concurrency effects now populated: `await_point` 67→**2419**, `lock_acquire`
    **60**, `lock_release` **60**, `thread_spawn` **27**, `channel_select` **3**
    (RC1 catalog × RC2 coverage).
  - F2 validated live for all three per-project crons (`symbol-extraction`,
    `call-graph`, `function-metrics` each echoed `project: pgmcp`, completed).
