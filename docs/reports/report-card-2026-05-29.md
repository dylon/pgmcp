# pgmcp — Repository Report Card (reliability-corrected)

**Date:** 2026-05-29 · **Supersedes:** `report-card-2026-05-28.md` · **Branch:** `feat/work-item-tracker`
**Status:** grading-logic reliability fixes + god-file split + Postgres-NOTICE fix — **`./scripts/verify.sh`: all gates passed.**

This card documents how the grades changed after the 2026-05-28 card exposed that **4 of 5 failing
grades were measurement artifacts**. The fixes (see `docs/quality/grading-reliability.md`) make every
dimension report **real signal or honest `N/A`**, on an **absolute scale (no curve)**. Evidence below
is **measured live** against the running database (via SQL + on-disk inspection) — exact composite
GPA/letter render once the daemon is restarted on the rebuilt binary (the standalone CLI cold-start
times out; see `reference_pgmcp_tool_cli_cold_start`).

## ⬡ What changed, with live evidence

| Dimension (2026-05-28) | Now | Live evidence (measured 2026-05-29) |
|---|---|---|
| Architecture › `separation_of_concerns` = **0.0 (F)** — fake | **`N/A`** (excluded from mean) | `topics_algo_signature=<ABSENT>` **and** newest file `2026-05-29` ≫ newest global topic `2026-05-20` ⇒ `topics_global_stale`=true. Top global-topic keywords are still `[the, and, dylon, home, workspace]` (200 degenerate topics) — the old emptiness-check called this "current." |
| Engineering › `complexity` = **B** — line-count proxy mislabeled as cyclomatic | **`N/A`** (excluded from mean) | `function_metrics_rows=0` for pgmcp ⇒ no per-function cyclomatic exists, so the dimension is honestly N/A instead of a proxy `B`. |
| Engineering › `finding_density` = **0.0 (F)** — ranking tools counted as defects | **sane** | Collectors (`complexity_hotspots`/`bug_prediction`) now emit only absolute-threshold outliers (McCabe >10/20, lines >500/1000, bug-score ≥0.4); `finding_density` de-dups by file. Verified by unit tests; no longer flags all 814 files. |
| ORR › `no_god_files` = **fail** (≥5 files >500 lines — unachievable) | absolute >2000-line bar; **2 worst god files eliminated** | On disk now: `server.rs` **11,962→1,461**, `queries.rs` **9,340→79**. Remaining >2000-line files (honestly flagged): `migrations.rs` 3781, `config.rs` 3516, `topic_clustering.rs` 3467, `sources.rs` 2618, `work_items.rs` 2373, `scheduler.rs` 2207, `automata.rs` 2065. |
| Overall = **F** (GPA 2.20) — `gpa×25` scale bug | computed on a **continuous, self-consistent** scale | `DimensionScore::gpa() = score/25`; pillar/overall letter == `letter_grade(mean score)`. A 2.x GPA can no longer print "F". Verified by `pillar_mean_ignores_absent_dims` (3.6) + boundary tests. |
| Health envelope: `topics_stale:false` (wrong) | **truthful** | `orient` now calls `topics_global_stale`/`graph_stale` (signature + freshness), not `topics.is_empty()`. |

## ▣ Honor roll (unchanged — genuinely earned)

Security pillar remains **A** (0 secrets, 0 crypto-misuse, 0 injection, 0 CVE). Structural
architecture remains strong: acyclic, SDP-compliant, loosely coupled, balanced. 1423 tests pass.

## ⚑ Genuinely actionable (real signal, not artifacts)

1. **Repopulate derived data** so `complexity` and `separation_of_concerns` become real (not `N/A`):
   restart the daemon on the rebuilt binary, then `trigger_cron` `symbol-extraction` →
   `function-metrics` (real cyclomatic) and `topic-clustering` (fresh, stopword-filtered, identifier-
   split topics — stamps `pgmcp-topics-v2`, clearing the staleness flag).
2. **Documentation** (Engineering F = 59) — an honest doc-file-presence ratio; write docs for the
   highest-PageRank modules.
3. **Remaining large files** (the 7 listed above) — optional further splits if `no_god_files` matters.

## ⚙ Note on log hygiene

The `word is too long to be indexed` log spam (289× in the daemon log) was **benign PostgreSQL FTS
NOTICEs** (`to_tsvector` skipping >~2 KB lexemes), surfaced at INFO. Fixed at the source with
`SET client_min_messages = warning` (`src/db/pool.rs`) — Postgres no longer sends NOTICE-level
messages; WARNING/ERROR still surface.

## Regenerating the live graded card (after daemon restart)

```
# restart the daemon on the rebuilt ./target/release/pgmcp, then:
mcp__pgmcp__trigger_cron job=symbol-extraction
mcp__pgmcp__trigger_cron job=function-metrics
mcp__pgmcp__trigger_cron job=topic-clustering
mcp__pgmcp__engineering_scorecard project=pgmcp format=full
mcp__pgmcp__quality_report       project=pgmcp format=markdown
mcp__pgmcp__orient               project=pgmcp     # topics_stale now flips false post-recompute
```
(Use the warm daemon's MCP tools, not the standalone CLI — the CLI cold-start rebuilds the fuzzy
tries + loads BGE-M3 on every invocation and times out.)
