# Scientific-Experiment Subsystem

pgmcp records — and arbitrates the methodology of — the experiments run while
developing across the workspace: performance **optimizations**, behavior-preserving
**feature refactors**, **feature additions**, **bug fixes**, and **diagnostic
deep-dives / root-cause investigations**. The structured record is the source of
truth that *renders* the committed `docs/scientific-ledger/*.md` ledgers, and every
experiment is searchable cross-project ("has anyone tried X? did it work?").

This document is the from-scratch reconstruction reference. The implementation plan
is `~/.claude/plans/plan-how-to-effectively-drifting-fox.md`.

## Architecture: the MCP server is the methodologist, the agent is the lab tech

Automating execution across heterogeneous projects is infeasible, so **agents run
the work**; but **the MCP server dictates the protocol and the data-collection
requirements**, validates conformance, and renders the verdict. The daemon never
executes arbitrary shell commands.

| Role | Responsibility |
|------|----------------|
| MCP server / tools | Prescribe the design for the experiment's `kind` (which metrics, sample size via power analysis, warm-up, statistical test, the *pre-registered* acceptance criterion, the data schema, a reproducibility checklist); validate submitted samples; run the frozen test; render and index the verdict. |
| Agent (Claude/Codex) | Execute the benchmark/profiling commands or invoke pgmcp's own analysis tools across git refs (directly, or via the `pgmcp experiment run` CLI helper that runs in the agent's shell), then submit raw samples. |

## Use-case taxonomy (`experiments.kind`)

`optimization | feature_refactor | feature_addition | bugfix | investigation | other`.
The kind selects the protocol archetype the server prescribes:

| Kind | Metrics | Source | Default criterion archetype |
|------|---------|--------|------------------------------|
| optimization | latency / throughput / RSS | `external_benchmark` (hyperfine/criterion/`time -v`) | Welch (or Mann-Whitney) `p<0.05` ∧ `\|Cohen's d\|≥0.5` ∧ correct direction, N≥30 |
| feature_refactor | perf no-regression + code-health (LCOM4, CK, coupling, complexity) + tests pass | perf=benchmark; structure=`pgmcp_metric`; tests=`agent_scalar` | `all_of`: TOST-equivalence (perf) ∧ directional/paired improvement (structure) ∧ absolute (tests=100%) |
| feature_addition | SLO + target outcome + no-regression | benchmark / `agent_scalar` / `pgmcp_metric` | `all_of`: absolute_threshold (SLO) ∧ improvement (target) ∧ TOST (existing paths) |
| bugfix | diagnose (root-cause chain) then verify (repro + no-regression) | `observational` evidence; `agent_scalar`; benchmark | per-hypothesis `observational`, then absolute + optional TOST |
| investigation / diagnostic deep-dive | counts/timings/flags/query results/log signatures | any | per-hypothesis `observational` verdict (supported/falsified); two-sample test if a distribution is collected |

### Metric nature → criterion type

- **Stochastic** (latency, throughput, accuracy across seeds) → repeated samples → significance test (Welch default). Welch `p<0.05` lives here.
- **Deterministic single-value** (LOC, one module's LCOM4) → threshold / relative-change criterion; no p-value.
- **Deterministic distribution-valued** (per-file complexity before/after) → **paired** Wilcoxon signed-rank, with Cliff's δ.

### Metric sources

`external_benchmark` (hyperfine/criterion/`/usr/bin/time -v`), `pgmcp_metric`
(`ck_metrics`, `lcom4`, `coupling_cohesion_report`, `complexity_hotspots`,
`design_metrics`, `test_coverage_gaps`, `mutation_score_surrogate`, `clone_density`,
`public_api_surface`, … run on a git ref), `agent_scalar` (test pass rate, accuracy,
recall@k, coverage%), `manual`.

### Hypothesis chains & evidence-based verdicts

Bug fixes and diagnostic deep-dives proceed as a **chain** of hypotheses, each
*supported* / *falsified* by recorded evidence (a query result, a log signature, a
reproduction) — the `observational` acceptance criterion, **no p-value**. The schema
holds many `experiment_hypotheses` per experiment, each with its own `verdict`
(`accepted`=supported, `rejected`=falsified, `inconclusive`). This captures *how
problems were diagnosed* — what was tried and ruled out — not just which fixes shipped.

## Established protocols adopted

- **W3C PROV-O** — provenance view emitted as `memory_relations` (`prov:wasGeneratedBy`/`used`/`wasAssociatedWith`/`wasDerivedFrom`/`wasInformedBy`); the run plan is a PROV `Plan`; the "repeat" loop edge is `wasInformedBy`.
- **MLflow** — `Experiment → Run → {params, metrics, artifacts}` schema shape; we add the pre-registered criterion + statistical decision MLflow lacks.
- **ML-Schema** (W3C CG) — vocabulary (Run/Task/EvaluationMeasure/EvaluationProcedure).
- **MLMD** (TFX) — typed Artifact/Execution/Context/Event discipline (every sample/result row knows its producing run).
- **Georges et al. 2007** (OOPSLA) + **Kalibera & Jones 2013** (ISMM) — CIs + non-parametric tests + raw-sample retention over mean±stddev; warm-up discard / steady state; repetition budget.
- **OSF / AsPredicted pre-registration** — `acceptance_criterion` frozen at `experiment_open` with `criterion_locked_at`; `experiment_decide` refuses a criterion edited after the first treatment sample.
- **FAIR** — stable ids, raw samples in Postgres, recorded commands/env/seed/SHA/hardware.

## Data model (`src/db/migrations.rs::ensure_experiment_tables`)

Seven tables + five enums (`experiment_kind`, `experiment_status`, `hypothesis_verdict`,
`experiment_arm_kind`, `effect_direction`). New tables are 1024d-direct
(`embedding vector(1024)`); HNSW via `build_hnsw_index` (`ensure_experiment_hnsw_index`).

- **experiments** — root: question/context, `kind`, project, status, hardware, links, bi-temporal supersession, `embedding` (title‖question‖context).
- **experiment_code_anchor** — file/chunk/topic anchor (mirrors `memory_code_anchor`).
- **experiment_hypotheses** — statement, `primary_metric`, `predicted_direction`, `acceptance_criterion JSONB` (frozen), `criterion_locked_at`, `planned_n`, `verdict`, `embedding`.
- **experiment_runs** — UUID PK; one arm execution; `command_spec`/`run_plan`/`host_meta` JSONB; `runner`; `seed`.
- **experiment_samples** — raw per-replicate samples; `is_warmup`; `unit_key` (per-file key for paired tests).
- **experiment_results** — the decision: `test_type`, `statistic`, `df`, `p_value`, `effect_size`, CI, `verdict`, `accepted`, `criterion_snapshot`, full `test_result` JSONB, `rationale`, `embedding` (rationale).
- **experiment_artifacts** — ad-hoc profiling/benchmark/debug capture (perf/hyperfine/criterion/massif/flamegraph/log); `experiment_id` NULL = free-standing.

Reconciliation with `agent_outcomes`: parallel-but-linked. `experiment_decide` may emit
a linked `agent_outcomes` row (via `a2a::best_practices::record_outcome`) so a confirmed
experiment can graduate into a durable mandate.

JSONB columns are bound as JSON text with a `$n::jsonb` cast (the crate's sqlx build has
no `json` feature); reads cast `col::text` and parse.

## Statistical engine (`src/stats/inference.rs`)

Self-contained; `statrs` supplies only the Student-t / normal / χ² CDFs & quantiles.

- `welford` (online mean/variance), `summarize`, `percentile`, `median`.
- `welch_t_test` (Welch–Satterthwaite df), `mann_whitney_u` (tie+continuity corrected normal approx), `wilcoxon_signed_rank` (paired).
- `bootstrap_diff_means`/`bootstrap_diff_medians` (percentile + BCa, seeded, reproducible).
- effect sizes: `cohens_d`, `hedges_g`, `cliffs_delta`, `rank_biserial`, `relative_change_median`.
- normality: `anderson_darling`, `dagostino_pearson`, `recommend_two_sample_test` (advisory — never overrides a pre-registered criterion).
- `tost_equivalence` (no-regression), `adjust_pvalues` (Bonferroni / Benjamini-Hochberg FDR), `required_n_per_arm` (Cohen power).

All results flow through a uniform `TestResult { kind, tail, statistic, df, p_value,
effect_size, effect_kind, ci_low/high, ci_level, n_*, notes }` (`p_value` is `NaN` for
non-NHST evidence).

## Acceptance criteria (`src/stats/acceptance.rs`)

`AcceptanceCriterion` (serde **adjacent**-tagged `{"type":…,"params":…}`, persisted as JSONB): `welch_t`, `mann_whitney_u`,
`wilcoxon_signed_rank`, `bootstrap_ci_excludes`, `effect_threshold`,
`relative_improvement`, `absolute_threshold`, `observational`, `equivalence`,
`all_of`/`any_of`/`not`. `evaluate(criterion, control, treatment, correction)` returns a
`Decision { accepted, rationale, evidence }`, threading the multiple-comparison
`correction` across the NHST leaves of a composite (BH FDR default). Default optimization
criterion: `welch_t{α=0.05, tail, min_effect=cohens_d 0.5}` with the correct tail.

## MCP tools (all in `src/mcp/tools/tool_experiments.rs`)

`experiment_open` (register + pre-register criterion + return the kind-aware protocol),
`experiment_protocol` (re/fetch the prescribed protocol), `experiment_record_measurement`
(submit raw samples, validated for conformance), `experiment_decide` (run the frozen test,
persist verdict, mirror to the memory graph, optional `agent_outcomes` link, optional
ledger render), `experiment_search` (cross-project), `experiment_get`/`experiment_list`/
`experiment_timeline`, `experiment_log_artifact`, `experiment_render_ledger`. Also wired
into `call_tool_cli` and the `pgmcp experiment …` / `pgmcp ledger …` CLI.

## Execution (`src/experiment/`, agent-driven)

`pgmcp experiment run` (in the agent's own process) fetches the prescribed protocol,
executes control/treatment arms × N replicates with warm-up, **CPU pinning** (one CCD
via `taskset`/`sched_setaffinity`), **governor check** (refuse non-`performance` when
configured), captures samples (native, or imported from hyperfine/criterion/`time -v`/
a `pgmcp_metric`), and auto-submits. The daemon only re-evaluates already-collected
samples; it never spawns arbitrary commands.

## Configuration (`[experiments]`)

`default_alpha` (0.05), `default_test` ("welch_t"), `default_power` (0.8),
`min_samples_per_arm` (30), `default_correction` ("benjamini_hochberg"),
`embed_on_write` (true), `auto_render_ledger` (false), `ledger_dir`
("docs/scientific-ledger"), `require_performance_governor` (true). Read live via
`ctx.config().load()`.

## Implementation status

| Phase | Scope | Status |
|-------|-------|--------|
| 0 | Boy-Scout: markdown heading-aware chunking + `memory_observations` embedding backfill | ✅ done |
| 1 | Statistical engine + acceptance taxonomy (`src/stats/{inference,acceptance}.rs`) + `statrs` | ✅ done (tests green) |
| 2 | Schema + kind-aware protocol + write path (migrations, config, stats counters, `experiment_open`/`_protocol`/`_record_measurement`) | ✅ implemented |
| 3 | Decide + memory-graph mirror (PROV) + cross-project search + read tools | ✅ implemented |
| 4 | Runner + CLI executor (`src/experiment/{spec,pinning,extract,runner}.rs` + `pgmcp experiment run|ingest`; CPU pinning + governor + hyperfine/criterion import) | ✅ implemented |
| 5 | Ledger render (`experiment_render_ledger` + `pgmcp ledger render`) + frontmatter parser + ad-hoc artifacts (`experiment_log_artifact`) + `pgmcp ledger import` | ✅ implemented |

All phases implemented; full `./scripts/verify.sh` gate (build + clippy -D warnings +
release test + gpu_smoke) is the final acceptance check. The 13 experiment MCP tools are
`experiment_open`, `experiment_protocol`, `experiment_record_measurement`,
`experiment_decide`, `experiment_search`, `experiment_get`, `experiment_list`,
`experiment_timeline`, `experiment_log_artifact`, `experiment_render_ledger` (+ the CLI
`pgmcp experiment run|ingest` and `pgmcp ledger render|import`).

The recursive `AcceptanceCriterion` uses serde **adjacent** tagging
(`#[serde(tag = "type", content = "params")]`); internal tagging on a recursive enum
stalls rustc's monomorphization collector into a multi-hour compile (empirically
reproduced — see ADR-006), so adjacent tagging is mandatory. (A crate-wide
`#![recursion_limit = "1024"]` remains, but for an unrelated reason: the large
`serde_json::json!` stats-snapshot literal in `src/stats/tracker.rs`.)
