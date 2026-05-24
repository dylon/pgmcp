# Formal verification index

Specs and proofs covering pgmcp's safety-critical state machines and
correctness invariants. Modeled after libgrammstein's
`formal/README.md` traceability table.

## TLA+ specs

| Spec                                          | Purpose                                                                                 | Code locus                                                  |
|-----------------------------------------------|-----------------------------------------------------------------------------------------|-------------------------------------------------------------|
| `tla/CronStateMachine.tla`                    | Cron task lifecycle: pending → running → completed; `DisjointSets` (every task in exactly one set); `NoDoubleProcessing`; crash-recovery `RecoverInProgressAsFailed`. | `src/cron/scheduler.rs` heavy-cron skip-gate; `src/stats/tracker.rs::CronJobOutcome` |
| `tla/SimilarityScanFkDrift.tla`               | Long-running cron's tolerance to file_chunks deletion mid-pass. Cached chunk_ids that become orphans must not produce FK violations on bulk INSERT. Models `ON DELETE CASCADE` of `similarities` rows when a chunk is deleted. | `src/cron/similarity.rs::run_similarity_scan`; pattern documented in `feedback_long_running_jobs_must_handle_fk_drift.md` |

## Rocq proofs

| Proof                                         | Theorem                                                                                  | Code locus                                                  |
|-----------------------------------------------|------------------------------------------------------------------------------------------|-------------------------------------------------------------|
| `rocq/TransducerMandateDedup.v`               | Idempotence + termination of the Phase 3 in-process mandate-dedup pipeline (`sessions::mark_near_duplicate_superseded`'s Transducer query → bulk UPDATE). | `src/sessions.rs::mark_near_duplicate_superseded`           |

## Status

| Property                                                                | Defined    | Mechanically checked | Verification log                                                  |
|-------------------------------------------------------------------------|------------|----------------------|-------------------------------------------------------------------|
| CronStateMachine — DisjointSets                                         | 2026-05-23 | 2026-05-23           | `states/cron_state_machine_tlc_2026-05-23.log` (TLC, exit 0)      |
| CronStateMachine — NoDoubleProcessing                                   | 2026-05-23 | 2026-05-23           | `states/cron_state_machine_tlc_2026-05-23.log` (TLC, exit 0)      |
| CronStateMachine — AtMostOneRunning                                     | 2026-05-23 | 2026-05-23           | `states/cron_state_machine_tlc_2026-05-23.log` (TLC, exit 0)      |
| CronStateMachine — LockMatchesRunning                                   | 2026-05-23 | 2026-05-23           | `states/cron_state_machine_tlc_2026-05-23.log` (TLC, exit 0)      |
| CronStateMachine — RecoverInProgressAsFailed (refinement, modeled in `Next`) | 2026-05-23 | 2026-05-23           | `states/cron_state_machine_tlc_2026-05-23.log` (TLC, exit 0)      |
| SimilarityScanFkDrift — NoOrphanFkInsert (under ON DELETE CASCADE)      | 2026-05-23 | 2026-05-23           | `states/similarity_scan_fk_drift_tlc_2026-05-23.log` (TLC, exit 0)|
| SimilarityScanFkDrift — CacheNeverExceedsKnownIds                       | 2026-05-23 | 2026-05-23           | `states/similarity_scan_fk_drift_tlc_2026-05-23.log` (TLC, exit 0)|
| TransducerMandateDedup — Idempotence (running twice yields same rows)   | 2026-05-23 | 2026-05-23           | `states/rocq_compile_2026-05-23.log` (`coqc`, exit 0)             |
| TransducerMandateDedup — Termination (`dedup_run` is structurally recursive over a finite list) | 2026-05-23 | 2026-05-23 | `states/rocq_compile_2026-05-23.log` (`coqc`, exit 0)             |

**P13.5 (2026-05-23) — Status integrity:** the previous version of
this README claimed "Verified 2026-05-23" without any mechanical
artefact under `states/`. P13.5 fixes both the proof (Rocq proof
rewritten with no `Hypothesis` / `Axiom` / `Admitted`, per
CLAUDE.md) and the README (status table now distinguishes "Defined"
from "Mechanically checked YYYY-MM-DD" + cites the log file).

## Inherited verification

pgmcp inherits five Coq/Rocq-verified theorems from `liblevenshtein-rust`
(used wherever Phase 3, Phase 5, Phase 7, Phase 10, P13.3, P13.4 reach
into the phonetic / Transducer framework):

| Theorem                  | What it guarantees                                                                                                    | Provenance                                                  |
|--------------------------|------------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------|
| Rule well-formedness     | Every Zompist rewrite rule has `length(pattern) ≥ 1` and bounded replacement length.                                   | `liblevenshtein-rust/docs/verification/phonetic/zompist_rules.v:285` |
| Bounded expansion        | `apply_rule_at r s pos = Some s' ⇒ length(s') ≤ length(s) + max_expansion_factor` (= 20).                              | `zompist_rules.v:424`                                       |
| Non-confluence           | Some rules do not commute → caller chooses order; system never claims order-independence.                              | `zompist_rules.v:491` (constructive counterexample)         |
| Termination              | `apply_rules_seq` with a well-formed rule set halts within bounded fuel.                                              | `zompist_rules.v:569`                                       |
| Idempotence              | The fixed point of `apply_rules_seq` is stable: re-applying the rule set to the normalized form yields the same form. | `zompist_rules.v:615`                                       |

These are surfaced to pgmcp users via the `phonetic_normalize` and
`phonetic_symbol_search` MCP tools: their result docstrings cite the
inherited guarantees so operators know the framework's canonical
form is a stable, terminating, bounded-memory operation.

## Compiling proofs

Per `~/.claude/CLAUDE.md`, Rocq compilation uses `systemd-run` to keep
memory and CPU bounded:

```bash
systemd-run --user --scope \
    -p MemoryMax=96G -p CPUQuota=1800% -p IOWeight=30 -p TasksMax=200 \
    coqc docs/formal/rocq/TransducerMandateDedup.v
```

TLA+ model checking uses TLC with auto worker count. Each `.tla` file
has a sibling `.cfg` declaring the constants and invariants:

```bash
cd docs/formal/tla
tlc -workers auto CronStateMachine.tla         \
    | tee ../states/cron_state_machine_tlc_$(date +%F).log
tlc -workers auto SimilarityScanFkDrift.tla    \
    | tee ../states/similarity_scan_fk_drift_tlc_$(date +%F).log
```

### `scripts/verify.sh` advisory gates

P13.5 wires `coqc` and `tlc` into `scripts/verify.sh` as advisory
gates. If the binaries are on `PATH` the gate runs and fails on
non-zero exit; if either is missing, the gate logs a clear "SKIP:
<tool> not found" line. Explicitly NOT silently skipped per
`feedback_feature_gated_build_verification.md`.
