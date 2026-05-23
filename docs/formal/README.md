# Formal verification index

Specs and proofs covering pgmcp's safety-critical state machines and
correctness invariants. Modeled after libgrammstein's
`formal/README.md` traceability table.

## TLA+ specs

| Spec                                          | Purpose                                                                                 | Code locus                                                  |
|-----------------------------------------------|-----------------------------------------------------------------------------------------|-------------------------------------------------------------|
| `tla/CronStateMachine.tla`                    | Cron task lifecycle: pending → running → completed; `DisjointSets` (every task in exactly one set); `NoDoubleProcessing`; crash-recovery `RecoverInProgressAsFailed`. | `src/cron/scheduler.rs` heavy-cron skip-gate; `src/stats/tracker.rs::CronJobOutcome` |
| `tla/SimilarityScanFkDrift.tla`               | Long-running cron's tolerance to file_chunks deletion mid-pass. Cached chunk_ids that become orphans must not produce FK violations on bulk INSERT. | `src/cron/similarity.rs::run_similarity_scan`; pattern documented in `feedback_long_running_jobs_must_handle_fk_drift.md` |

## Rocq proofs

| Proof                                         | Theorem                                                                                  | Code locus                                                  |
|-----------------------------------------------|------------------------------------------------------------------------------------------|-------------------------------------------------------------|
| `rocq/TransducerMandateDedup.v`               | Idempotence + termination of the Phase 3 in-process mandate-dedup pipeline (`sessions::mark_near_duplicate_superseded`'s Transducer query → bulk UPDATE). | `src/sessions.rs::mark_near_duplicate_superseded`           |

## Status

| Property                                                                | Verification              | Reviewed |
|-------------------------------------------------------------------------|---------------------------|----------|
| CronStateMachine — DisjointSets                                         | TLA+ inductive invariant  | 2026-05-23 |
| CronStateMachine — NoDoubleProcessing                                   | TLA+ inductive invariant  | 2026-05-23 |
| CronStateMachine — RecoverInProgressAsFailed                            | TLA+ refinement           | 2026-05-23 |
| SimilarityScanFkDrift — WHERE EXISTS pattern prevents orphan FK fail    | TLA+ inductive invariant  | 2026-05-23 |
| TransducerMandateDedup — Idempotence (running twice yields same rows)   | Rocq theorem              | 2026-05-23 |
| TransducerMandateDedup — Termination (n-best query halts on finite DAWG)| Rocq theorem              | 2026-05-23 |

## Inherited verification

pgmcp inherits five Coq/Rocq-verified theorems from `liblevenshtein-rust`
(used wherever Phase 3, Phase 5, Phase 7, Phase 10 reach into the
phonetic / Transducer framework):

| Theorem                  | What it guarantees                                                                                                    | Provenance                                                  |
|--------------------------|------------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------|
| Rule well-formedness     | Every Zompist rewrite rule has `length(pattern) ≥ 1` and bounded replacement length.                                   | `liblevenshtein-rust/docs/verification/phonetic/zompist_rules.v:285` |
| Bounded expansion        | `apply_rule_at r s pos = Some s' ⇒ length(s') ≤ length(s) + max_expansion_factor` (= 20).                              | `zompist_rules.v:424`                                       |
| Non-confluence           | Some rules do not commute → caller chooses order; system never claims order-independence.                              | `zompist_rules.v:491` (constructive counterexample)         |
| Termination              | `apply_rules_seq` with a well-formed rule set halts within bounded fuel.                                              | `zompist_rules.v:569`                                       |
| Idempotence              | The fixed point of `apply_rules_seq` is stable: re-applying the rule set to the normalized form yields the same form. | `zompist_rules.v:615`                                       |

These are surfaced to pgmcp users via the `phonetic_normalize` and
`phonetic_symbol_search` MCP tools (Phase 10): their result docstrings
cite the inherited guarantees so operators know the framework's
canonical form is a stable, terminating, bounded-memory operation.

## Compiling proofs

Per `~/.claude/CLAUDE.md`, Rocq compilation uses systemd-run to keep
memory and CPU bounded:

```bash
systemd-run --user --scope \
    -p MemoryMax=96G -p CPUQuota=1800% -p IOWeight=30 -p TasksMax=200 \
    make -C docs/formal/rocq -j1
```

TLA+ model checking uses TLC with auto worker count:

```bash
cd docs/formal/tla && tlc CronStateMachine.tla -workers auto
```
