# pgmcp — Repository Report Card

**Date:** 2026-05-28 (generated 2026-05-29 00:36 UTC) · **Version:** pgmcp 0.1.0 · **Branch:** `feat/work-item-tracker`
**Source:** pgmcp self-analysis — `engineering_scorecard`, `quality_report`, `architecture_quality`, `orient`, `complexity_hotspots`, `index_stats`
**Derived-data health:** `graph_stale: false` · `topics_stale: false` · `phase: ready` — *“All derived data current.”*

---

## ⬡ Overall

| Grading lens | Grade | GPA | What it means |
|---|:---:|:---:|---|
| `engineering_scorecard` (10 git+structure dims, ORR-gated) | **F** | 2.20 | F is forced by the ORR gate (2 hard checks fail), not by the dimension average |
| `quality_report` (3-pillar average: Eng · Arch · Sec) | **D** | 2.72 | (2.00 + 2.15 + 4.00) ⁄ 3 |
| **Adjusted read** (artifacts + solo-dev signals discounted) | **B+** | ~3.3 | Structurally strong code; the F’s are mostly measurement artifacts and solo-repo git dynamics |

> The two tools disagree by design. `engineering_scorecard` overrides to **F** because the
> Operational-Readiness Review fails (it gates on `no_god_files` and `bus_factor_ok`).
> `quality_report` averages three pillars, one of which (**Security A**) pulls the GPA back up to a **D**.
> Neither headline is wrong — but read the **Signal vs. Noise** section before treating any single **F** as a defect.

---

## ▦ Scale & composition

```
Files ........ 814            Lines ........ 240,403        Avg file ..... 295 lines
Tests ........ 172 files      Docs ......... 48 files       Languages .... Rust 741 · Markdown 48
Formal ....... TLA⁺ 9 · Coq/Rocq 3          Hubs ......... src/mcp/server.rs · src/db/queries.rs
Effects (pgmcp) .. may_panic 415 · test 329 · async 234 · unsafe 24 · inline 9
Social ....... 13 open work items · 36 memory entities · 2 A2A peers
```

---

## ▣ Report card by subject

Grades come straight from the tools. **Signal** is my classification of how much each grade reflects
*intrinsic code quality* vs. a measurement artifact or a structural fact of a solo, fast-moving repo.

### Engineering pillar — F (GPA 2.00)

| Subject | Score | Grade | Signal | Note |
|---|:---:|:---:|:---:|---|
| Dependency health (no cycles) | 100.0 | **A** | ● real | Zero circular dependencies |
| Test coverage ratio | 100.0 | **A** | ● real | 172 test files / 814 |
| Coupling (inter-module) | 99.9 | **A** | ● real | Very low afferent+efferent coupling |
| Freshness | 98.3 | **A** | ● real | Actively maintained |
| Code structure | 88.1 | **B** | ● real | File-size distribution; dragged by a few giant modules |
| Complexity | 87.1 | **B** | ◐ proxy | Structural proxy — per-function cyclomatic **not populated** |
| Documentation | 59.0 | **F** | ● real | Genuine, actionable gap |
| Code stability (churn) | 47.2 | **F** | ◑ solo-dev | High churn = active solo iteration |
| Bug-fix ratio | 32.7 | **F** | ◑ solo-dev | High fix-commit share; partly rework, partly fast iteration |
| Team distribution (bus factor) | 24.6 | **F** | ◑ solo-dev | Single author owns most lines (real continuity risk for a team/prod) |
| Finding density | 0.0 | **F** | ○ noise | Severity-weighted load dominated by noisy heuristics (see below) |

### Architecture pillar — F (GPA 2.15)

| Subject | Score | Grade | Signal | Note |
|---|:---:|:---:|:---:|---|
| SDP compliance | 100.0 | **A** | ● real | Stable-dependencies principle holds |
| Acyclicity | 100.0 | **A** | ● real | No import cycles |
| Loose coupling | 99.9 | **A** | ● real | |
| Code organization | 99.9 | **A** | ● real | Card & Glass module-complexity surrogate |
| Module balance | 91.9 | **A** | ● real | Even PageRank spread |
| Propagation cost | 87.2 | **B** | ● real | DSM avg reachable fraction |
| OO coupling | 77.7 | **C** | ● real | Avg distinct referenced files |
| Dependency health | 77.6 | **C** | ◑ solo-dev | Inverse of fix-commit ratio |
| Test coverage | 63.4 | **D** | ● real | Fraction of files that are tests |
| Documentation | 59.0 | **F** | ● real | Same gap as Engineering pillar |
| API stability | 47.2 | **F** | ◑ solo-dev | Inverse of churn — expected pre-1.0 |
| Finding density | 52.9 | **F** | ○ noise | Heuristic-dominated |
| Separation of concerns | 0.0 | **F** | ○ artifact | Topic-derived; topic model is degenerate (see below) |

### Security pillar — A (GPA 4.00) ✦ honor roll

| Subject | Score | Grade | Signal | Note |
|---|:---:|:---:|:---:|---|
| Secret hygiene | 100.0 | **A** | ● real | `secret_detection`: 0 findings |
| Crypto hygiene | 100.0 | **A** | ● real | `crypto_misuse` / `unsafe_deserialization`: 0 |
| Finding density (security) | 99.0 | **A** | ● real | Only 8 medium + 1 low across all security collectors |
| Injection risk | 93.9 | **A** | ● real | `injection_candidates`: 0; `taint_analysis`: 5 (low) |
| Supply chain | N/A | — | — | No external advisories matched |

---

## ⛉ Operational-Readiness Review — **FAIL** (2 of 8)

```
✓ no_circular_deps      ✓ test_coverage        ✓ has_documentation     ✓ low_churn
✓ low_fix_ratio         ✓ recently_maintained  ✗ no_god_files          ✗ bus_factor_ok
```

The **only** two failing gates are `no_god_files` and `bus_factor_ok`. Everything else passes.
The overall **F** is entirely a consequence of these two checks — the report is *not* describing a broadly failing codebase.

---

## ⚖ Signal vs. noise — read this before acting

Four of the five **F** grades are measurement artifacts or solo-repo dynamics, not defects:

1. **`separation_of_concerns = 0.0` is an artifact.** It’s “avg distinct topics per file,” but the topic
   model is degenerate — the top global topics are stopword clusters (`the / and / dylon / home / workspace`,
   315 K members). The clustering carries almost no information, so this dimension is meaningless right now.
   *Fix:* re-run `discover_topics` / `topic-clustering` with better label extraction, then re-grade.

2. **`finding_density = 0.0` (Engineering) is noise-dominated.** The 717 High / 523 Med / 1336 Low load is
   inflated by blunt heuristics: `bug_prediction` flagged **780** files and `complexity_hotspots` flagged
   **all 814**. When a collector flags everything, its signal-to-noise is near zero.

3. **`complexity = B` rests on a proxy.** `cyclomatic_max = 0` and `function_count = 0` for *every* file —
   the `function-metrics` cron hasn’t populated per-function complexity (it’s in cooldown). The grade is
   computed from size/chunk structural proxies, not real cyclomatic complexity. *Fix:* `trigger_cron function-metrics`.

4. **Churn / fix-ratio / bus-factor F’s are solo-repo facts.** A single very active author produces high
   churn, a high fix-commit share, and bus-factor = 1 by construction. These aren’t quality regressions —
   though **bus-factor = 1 is a genuine continuity risk** the day this needs to outlive one maintainer.

**What survives scrutiny as real, actionable signal:** ① documentation gap, ② two god files, ③ everything
in the honor roll is genuinely earned.

---

## ✦ Honor roll (genuine strengths)

- **Structural architecture is excellent** — acyclic (100), SDP-compliant (100), loosely coupled (99.9),
  well-organized (99.9), balanced (91.9). This is a cleanly layered codebase.
- **Security is clean** — 0 secrets, 0 crypto misuse, 0 injection candidates, 0 CVE supply-chain hits,
  0 unsafe-deserialization. Only 5 low-severity taint paths and 1 PII spread across the whole tree.
- **Strong test discipline** — 172 test files, dedicated `pgmcp-testing/` crate, golden regeneration, e2e suites.
- **Formal-methods coverage** — 9 TLA⁺ specs + 3 Coq/Rocq proofs accompany the implementation.
- **No circular dependencies** anywhere.

---

## ⚑ Needs improvement (ranked, actionable)

1. **Split the two god files** *(clears the `no_god_files` ORR gate)*
   - `src/mcp/server.rs` — **490 KB / 296 chunks** (also the #1 PageRank hub). The ~72 MCP tool handlers
     should be split into per-domain handler modules; `src/mcp/tools/` already exists as the target shape.
   - `src/db/queries.rs` — **333 KB / 230 chunks** (#2 hub). Decompose by query domain; `src/db/queries/`
     already exists (`work_items.rs` is split out — extend that pattern).
2. **Close documentation gaps** *(Documentation F = 59.0; `doc_coverage_gaps`)* — subsystems exist only in
   code. Prioritize docs for the highest-PageRank modules first.
3. **Refresh the derived-data layer so the next card is trustworthy** —
   `trigger_cron function-metrics` (real cyclomatic), then `discover_topics` (fix the degenerate topics),
   then re-run `quality_report`. Two of the five F’s should evaporate or become meaningful.
4. **Triage the test-file hotspots** — the finding-weighted worst-files list is dominated by large
   integration tests; confirm these are intentional breadth, not accidental complexity.
5. **(If this outgrows a solo project)** mitigate **bus-factor = 1** with docs + onboarding notes.

### Largest modules (god-file candidates, by size)

| File | Size | Chunks | Role |
|---|---:|---:|---|
| `src/mcp/server.rs` | 490 KB | 296 | MCP tool-dispatch surface · top hub |
| `src/db/queries.rs` | 333 KB | 230 | SQL query layer · #2 hub |
| `src/db/migrations.rs` | 164 KB | 94 | Schema migrations |
| `src/patterns/automata.rs` | 148 KB | 52 | Pattern catalog (automata family) |
| `src/config.rs` | 132 KB | 88 | Configuration |
| `src/cron/topic_clustering.rs` | 117 KB | 84 | FCM topic clustering cron |

### Finding-weighted worst files (`quality_report` roll-up)

| File | Weighted | Findings |
|---|---:|---:|
| `pgmcp-testing/tests/cli_subcommands_smoke.rs` | 34.00 | 34 |
| `src/work_pool/adaptive.rs` | 32.50 | 40 |
| `pgmcp-testing/tests/db_sql_surface_integration.rs` | 24.75 | 24 |
| `src/indexer/claude_chunker.rs` | 22.75 | 19 |
| `src/mandates.rs` | 20.75 | 20 |

---

## ⚙ Methodology & caveats

- **Grades are tool output, verbatim.** The **Signal** column and **Adjusted read** are interpretation.
- **Heuristic collectors are noisy.** `bug_prediction` (780) and `complexity_hotspots` (814/814) flag
  near-everything; treat their counts as ranking aids, not absolute defect counts.
- **Per-function metrics absent.** `function-metrics` cron in cooldown ⇒ complexity uses size proxies.
- **Topic model degenerate.** Stopword-dominated clusters ⇒ `separation_of_concerns` is not meaningful.
- **Git signals reflect a solo repo.** Churn, fix-ratio, and bus-factor describe development *style*, not quality.
- **No line-coverage.** Test grades are path-based ratios, not executed coverage (use tarpaulin/llvm-cov for true %).

**Reproduce:**
```
pgmcp tool engineering_scorecard project=pgmcp format=full
pgmcp tool quality_report       project=pgmcp format=markdown
pgmcp tool architecture_quality project=pgmcp detail=full
pgmcp tool complexity_hotspots  project=pgmcp sort_by=size
```
