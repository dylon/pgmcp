# Formal verification index

Specs and proofs covering pgmcp's safety-critical state machines and
correctness invariants. Modeled after libgrammstein's
`formal/README.md` traceability table.

## TLA+ specs

| Spec                                          | Purpose                                                                                 | Code locus                                                  |
|-----------------------------------------------|-----------------------------------------------------------------------------------------|-------------------------------------------------------------|
| `tla/CronStateMachine.tla`                    | Cron task lifecycle: pending → running → completed; `DisjointSets` (every task in exactly one set); `NoDoubleProcessing`; crash-recovery `RecoverInProgressAsFailed`. | `src/cron/scheduler.rs` heavy-cron skip-gate; `src/stats/tracker.rs::CronJobOutcome` |
| `tla/SimilarityScanFkDrift.tla`               | Long-running cron's tolerance to file_chunks deletion mid-pass. Cached chunk_ids that become orphans must not produce FK violations on bulk INSERT. Models `ON DELETE CASCADE` of `similarities` rows when a chunk is deleted. | `src/cron/similarity.rs::run_similarity_scan`; pattern documented in `feedback_long_running_jobs_must_handle_fk_drift.md` |
| `tla/CfsmNetwork.tla`                          | ADR-009 Deliberation CFSM network (O/R/T): synchronous product of the projected machines — sender-driven choice + bounded loop + a bystander. `DeadlockFreedom`, `NoOrphan`, `RoundsBounded`, `EventualTermination`. | `src/csm/machine.rs`, `src/csm/transition.rs`, `src/csm/examples.rs` |
| `tla/A2aLinearPipeline.tla`                    | ADR-009 linear patterns — Sequential / Mixture / Distillation / Recursive via per-pattern `.cfg` (`NStages`). Bounded request/response pipeline; `DeadlockFreedom`, `EventualTermination`. | `src/csm/registry.rs` |
| `tla/RmasRecursionLoop.tla`                    | ADR-009 Track B latent-decode discipline: `LatentNeverDecodedMidLoop` — text is produced only at the final round's last agent. | `src/csm/role.rs::MessageMedium` |

## Rocq proofs

| Proof                                         | Theorem                                                                                  | Code locus                                                  |
|-----------------------------------------------|------------------------------------------------------------------------------------------|-------------------------------------------------------------|
| `rocq/TransducerMandateDedup.v`               | Idempotence + termination of the Phase 3 in-process mandate-dedup pipeline (`sessions::mark_near_duplicate_superseded`'s Transducer query → bulk UPDATE). | `src/sessions.rs::mark_near_duplicate_superseded`           |
| `rocq/CsmMpst.v`                              | ADR-009 CSM/MPST metatheory (one self-contained file — `verify.sh` runs `coqc` per file). T1 RLM termination; T2 deliberation termination; T3 global progress; T4 subject reduction; T5 projection soundness (send/recv/bystander **and** choice sender/receiver/bystander); T6 operational correspondence (bidirectional gstep↔lstep) — all **∀ finite well-formed `G` including `GChoice`** (the plain external-choice merge is the `bystanders_agree` side-condition); `protocol_fidelity` is the choice-free corollary. No `Admitted`/`Axiom`/`Hypothesis`. | `src/csm/mpst/{global,local,project}.rs`, `src/a2a/rlm.rs` |
| `rocq/CsmMedium.v`                            | ADR-009 Phase R1 medium discipline: `medium_discipline` (a well-media-formed protocol never places a black-box role on a latent edge) + the contrapositive corollary. No `Admitted`/`Axiom`. | `src/csm/media.rs::check_media_discipline` |

## ADR-009 formal artefacts: complete

Both layers are done — the TLA⁺ specs (Phase 4, all TLC-checked) and the Rocq
metatheory `CsmMpst.v` (Phase 5, T1–T6 to `Qed`). See the Status table.

**On TLAPS:** the TLA⁺ `THEOREM Spec ⇒ []Invariants` statements are present in
each spec but discharged by **TLC** (finite, mechanical) rather than `tlapm` —
TLAPS is not packaged for this host, and the *unbounded* (∀-size) guarantees are
proven independently in the Rocq metatheory (a stronger, general result). The
`THEOREM`s remain tlapm-ready if that prover is installed.

**Scope (honest):** T1–T6 hold for all finite well-formed global types, **including
the `GChoice` (Deliberation) case** — projection is general (sender → `LSelect`,
receiver → `LBranch`, bystander → the merged branch), and the external-choice plain
merge is encoded as the `bystanders_agree` Prop side-condition (every branch
projects identically for a bystander), so `project` stays a total `Fixpoint` with no
decidable-equality/merge machinery. T6 is bidirectional gstep↔lstep correspondence
stated per head construct (`GMsg`/`GChoice` are the only steppable constructors), so
"no behaviour lost or invented" is mechanical, not just TLC-/test-checked. The merge
discipline follows the established MPST result (Honda-Yoshida-Carbone POPL'08;
Scalas-Yoshida POPL'19). The one residual modeling boundary: global types are finite
unrolled trees (bounded runs), matching what pgmcp actually validates — recursion is
the separate well-founded T1/T2 argument, not coinduction.

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
| CfsmNetwork (Deliberation) — Invariants + DeadlockFreedom + EventualTermination | 2026-05-27 | 2026-05-27 | `states/cfsm_network_tlc_2026-05-27.log` (TLC, no error; 19 states) |
| A2aLinearPipeline — Sequential (NStages=3) — Invariants + DeadlockFreedom + EventualTermination | 2026-05-27 | 2026-05-27 | `states/a2asequential_tlc_2026-05-27.log` (TLC, no error)         |
| A2aLinearPipeline — Mixture (NStages=4)                                 | 2026-05-27 | 2026-05-27 | `states/a2amixture_tlc_2026-05-27.log` (TLC, no error)            |
| A2aLinearPipeline — Distillation (NStages=2)                            | 2026-05-27 | 2026-05-27 | `states/a2adistillation_tlc_2026-05-27.log` (TLC, no error)       |
| A2aLinearPipeline — Recursive (NStages=2)                               | 2026-05-27 | 2026-05-27 | `states/a2arecursiverlm_tlc_2026-05-27.log` (TLC, no error)       |
| RmasRecursionLoop — LatentNeverDecodedMidLoop + EventualTermination     | 2026-05-27 | 2026-05-27 | `states/rmas_recursion_loop_tlc_2026-05-27.log` (TLC, no error)   |
| CsmMpst — T1 `rlm_terminates` (RLM self-recursion well-founded)         | 2026-05-27 | 2026-05-27 | `coqc CsmMpst.v` exit 0 (no `Admitted`/`Axiom`)                   |
| CsmMpst — T2 `deliberation_terminates` (bounded loop well-founded)      | 2026-05-27 | 2026-05-27 | `coqc CsmMpst.v` exit 0                                            |
| CsmMpst — T3 `global_progress` (non-End wf global type steps)           | 2026-05-27 | 2026-05-27 | `coqc CsmMpst.v` exit 0                                            |
| CsmMpst — T4 `wf_preserved` (subject reduction)                         | 2026-05-27 | 2026-05-27 | `coqc CsmMpst.v` exit 0                                            |
| CsmMpst — T5 projection soundness: GMsg `project_send/recv_sound`+`project_bystander` **and** GChoice `project_choice_sender/receiver/bystander` (∀ wf `G`) | 2026-05-27 | 2026-05-27 | `coqc CsmMpst.v` exit 0                                            |
| CsmMpst — T6 operational correspondence: `projection_sound` (soundness) + `gstep_iff_sender_lstep_{msg,choice}` (bidirectional) + `sound_choice_bystander` (merge) — incl. `GChoice`; `protocol_fidelity` choice-free corollary | 2026-05-27 | 2026-05-27 | `coqc CsmMpst.v` exit 0                                            |
| CsmMedium — `medium_discipline` (black-box roles are Text-only)         | 2026-05-27 | 2026-05-27 | `coqc CsmMedium.v` exit 0                                          |

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
