# Grading reliability: making pgmcp's scorecards trustworthy

This document records the corrections made to pgmcp's quality-grading logic so its
grades reflect **real signal**, not measurement artifacts. The driving principle,
set by the maintainer, is: **every dimension must measure something real (or be
honestly `N/A`), and grading uses absolute, fixed thresholds — never a curve.**

## Background

A self-analysis report card graded pgmcp `F / D` overall. Investigation showed
**4 of the 5 failing grades were artifacts**, not code defects:

| Grade | Was reported | Real cause |
|---|---|---|
| `separation_of_concerns` | 0.0 (F) | Stale, degenerate topic model (labels were stopwords/paths) |
| `finding_density` | 0.0 (F) | Ranking tools counted as defects (every file "flagged") |
| `complexity` | B | A `line_count > 500` proxy presented as if it were cyclomatic |
| overall letter | F | A GPA→letter scale-mismatch (`gpa × 25`) that made GPA 2.2 an "F" |

## The corrections

### 1. Continuous, self-consistent grade scale (no curve)
`DimensionScore::gpa()` (`src/quality/report.rs`) now maps a 0–100 score to a 4.0
GPA **continuously** (`score / 25`, clamped) instead of the lossy
score→letter→GPA bucketing (which collapsed 59 and 24 both to `F`/0.0). This makes
the pillar/overall GPA proportional to the underlying scores, and makes
`gpa_letter(mean gpa) == letter_grade(mean score)` — a single absolute scale
(90/80/70/60). The GPA a tool reports and the letter it prints can no longer
contradict each other. Both scorecards divide by the count of **scorable** dims
(absent dims are excluded, not counted as 0).

### 2. Absolute finding thresholds (no "flag everything")
`finding_density` (`src/quality/report.rs`) now **de-duplicates by file**, counting
each file once at its worst severity, so a ranking-style collector can no longer
saturate the density to 0. The collectors themselves
(`src/quality/collectors/code_health.rs`) now emit a finding only when a file
crosses an **absolute, criterion-referenced** bar — never one per file, never a
per-project max-normalized curve:
- `complexity_hotspots`: McCabe cyclomatic bands (`>20` High, `>10` Medium) and
  file-size bands (`>1000` / `>500` lines); healthy files produce no finding.
- `bug_prediction`: only Medium+ defect-proneness (`>= 0.4`) is a finding.

### 3. Real complexity, or honest `N/A`
`engineering_scorecard`'s complexity dimension (`tool_engineering_scorecard.rs`)
now reads worst-function **cyclomatic** complexity from `function_metrics`
(threshold `>15`, absolute). When per-function metrics have not been computed yet
it returns `DimensionScore::absent(...)` (renders `N/A`, excluded from the mean)
rather than silently substituting a line-count proxy.

### 4. `no_god_files` measures genuine outliers
The ORR gate was `>= 5 files over 500 lines` — unachievable for any sizable repo
and not a god-file signal. It is now an absolute outlier bar (`line_count > 2000`),
passing only when no file exceeds it.

### 5. Topic-derived grade is `N/A` when topics are unreliable
`architecture_quality`'s `separation_of_concerns` returns `N/A` when the global
topic model is stale or absent, instead of a misleading 0.0. "Stale" is now a
**real** check (below), not "is the result set empty?".

### 6. Honest staleness detection
`orient`'s health envelope previously set `topics_stale` from `topics.is_empty()`
— so ancient, degenerate topics read as "current." It now uses
`db::queries::topics_global_stale` / `graph_stale`, which compare a stored
**algorithm signature** (`pgmcp_metadata['topics_algo_signature']` vs
`cron::topic_clustering::TOPICS_ALGO_SIGNATURE`) and `computed_at` against the
newest indexed file. Topics computed by older tokenizer/label code carry no
signature and are correctly flagged stale.

### 7. Process/team metrics: honest, kept, not curved
`team_distribution`, `code_stability` (churn), `bug_fix_ratio`, and the ORR
`bus_factor_ok` gate are retained as honest absolute signals. A solo repo
legitimately scores low on bus factor — that is reliable signal, not a defect to
hide. Correction #1 ensures these contribute proportionally rather than through
the broken `gpa × 25` path.

## Topic-model quality (so labels are meaningful)

The degenerate `the / and / dylon / home / workspace` labels were **stale data**:
the c-TF-IDF path already filters stopwords, but the stored topics predated the
stopword tiers. The fixes (`src/cron/topic_clustering.rs`):
- **Auto-derived username stopword** from `$USER`/`$HOME` (kills the `dylon` leak
  without manual configuration).
- **Identifier splitting** (`split_identifier`): `tokenize_query` → `tokenize`,
  `query`; `parseHTTPResponse` → `parse`, `http`, `response`. Concept words, not
  compound tokens.
- **df cutoff**: drop words appearing in >40% of topics (≥5 topics) — strengthens
  separation, applied on both the in-memory and streaming (global) paths.
- **Degeneracy guard**: `store_topics` stamps the algorithm signature only for a
  healthy global topic set; collapsed/repeated labels are logged and left
  unstamped so the model is treated as stale and recomputed.

c-TF-IDF with this preprocessing is a BERTopic-class representation. An optional
further refinement — embedding-based keyword selection (KeyBERT) with MMR
diversification over the BGE-M3 chunk embeddings — is a natural next layer; it
requires threading an embedder handle into the clustering cron.

## Operational notes

- **Repopulating per-function metrics**: `function_metrics` is empty after a recent
  daemon restart (the heavy cron chain has ~30–40 min ready-delays). It populates
  in steady state, or on demand via `trigger_cron job="symbol-extraction"` then
  `job="function-metrics"` (these bypass the cooldown). Until then, `complexity`
  honestly reports `N/A`.
- **Recomputing topics**: `trigger_cron job="topic-clustering"` after a rebuild;
  the new signature is stamped only if the result is non-degenerate.

## Regenerating the report card

```
trigger_cron job="symbol-extraction"
trigger_cron job="function-metrics"
trigger_cron job="topic-clustering"
# then:
pgmcp tool engineering_scorecard project=pgmcp format=full
pgmcp tool quality_report       project=pgmcp format=markdown
pgmcp tool orient               project=pgmcp     # health envelope now truthful
```
Grades should now be reliable: real or `N/A`, absolute, and internally consistent.
