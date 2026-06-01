(*
 * ADR-011 — soundness of the shadow-ASR lock-order deadlock-detection method.
 *
 * `tool_deadlock_cycles` (src/concurrency/, src/graph/lock_order.rs) builds the
 * interprocedural LOCK-ORDER graph R — an edge A→B iff some function acquires B
 * while holding A (computed over the ordered per-function sync_ops skeleton,
 * inlined across the resolved call graph) — runs Tarjan SCC, and flags any cycle
 * as a deadlock candidate (Havender 1968; Coffman, Elphick & Shoshani 1971, the
 * four conditions, of which circular-wait is #4). THIS FILE proves the soundness
 * of that test:
 *
 *     acyclic R  ⇒  no circular wait  ⇒  deadlock-free.
 *
 * The reduction is PROVED, not assumed: the wait-for relation realizable in any
 * reachable state is a sub-relation of R (`wait_for_subset_lockorder`), built
 * from the operational fact that a process holding A and requesting B induces
 * the lock-order edge A→B (the `respects` invariant of the ordered skeleton). A
 * wait-for cycle would then be an R⁺ self-loop, which `acyclic R` forbids
 * (`cycle_in_acyclic_is_false`). Acyclicity is therefore a SOUND *sufficient*
 * condition for deadlock-freedom; the converse is NOT claimed (a cycle is a
 * candidate, not a proof — over-approximation; see ADR-011 on false positives).
 *
 * Single self-contained file; Rocq 9.x Stdlib only; runs under verify.sh's
 * per-file `coqc` gate. No `Admitted` / `Axiom` / `Hypothesis` (CLAUDE.md).
 * Section `Variable`s are universally-quantified parameters, not axiomatic
 * assumptions (the TransducerMandateDedup.v / CsmMpst.v posture). Every
 * induction enumerates all constructors.
 *)

From Stdlib Require Import Relations.Relation_Definitions.
From Stdlib Require Import Relations.Relation_Operators.
From Stdlib Require Import List.
Import ListNotations.

Section LockOrder.

  (* Lock resources (e.g. the best-effort resource_key strings). *)
  Variable Resource : Type.

  (* The lock-order relation: A ~> B iff some function acquires B while holding
     A. Supplied by the extraction/analysis; here a parameter. *)
  Variable R : relation Resource.

  (* Transitive closure (Stdlib clos_trans, constructors t_step / t_trans). *)
  Definition Rplus := clos_trans Resource R.

  (* ACYCLICITY: no resource reaches itself by a nonempty R-path — exactly "the
     lock-order graph is a DAG" (no nontrivial SCC). *)
  Definition acyclic : Prop := forall x : Resource, ~ Rplus x x.

  Lemma R_in_Rplus : forall x y, R x y -> Rplus x y.
  Proof. intros x y H. apply t_step. exact H. Qed.

  Lemma Rplus_trans : transitive Resource Rplus.
  Proof. intros x y z Hxy Hyz. eapply t_trans; eauto. Qed.

  (* If Wr ⊆ R⁺ then Wr⁺ ⊆ R⁺ (R⁺ is transitively closed). Induction over the
     two clos_trans constructors. *)
  Lemma clos_trans_mono_into_Rplus :
    forall (Wr : relation Resource),
      inclusion Resource Wr Rplus ->
      inclusion Resource (clos_trans Resource Wr) Rplus.
  Proof.
    intros Wr Hsub x y H.
    induction H as [x y Hstep | x y z _ IHxy _ IHyz].
    - apply Hsub. exact Hstep.
    - eapply Rplus_trans; eauto.
  Qed.

  (* A cycle through x in a sub-relation of R⁺ contradicts acyclicity. *)
  Lemma cycle_in_acyclic_is_false :
    forall (Wr : relation Resource) (x : Resource),
      acyclic ->
      inclusion Resource Wr Rplus ->
      clos_trans Resource Wr x x ->
      False.
  Proof.
    intros Wr x Hacyc Hsub Hcyc.
    apply (Hacyc x).
    apply (clos_trans_mono_into_Rplus Wr Hsub x x Hcyc).
  Qed.

  (* ---- The operational layer: processes, held locks, the wait-for graph. ---- *)

  (* A process state: the locks it currently holds and the lock it next requests
     (its position in the ordered acquisition skeleton). *)
  Record PState : Type := { held : list Resource ; req : Resource }.

  (* Skeleton-respect: every currently-held lock A precedes the requested lock in
     the order, i.e. R A (req) — because A was acquired earlier in the SAME
     ordered skeleton than the about-to-be-acquired req, and R records exactly
     that held→requested pair. This is the extraction's structural guarantee. *)
  Definition respects (s : PState) : Prop :=
    forall A, In A (held s) -> R A (req s).

  (* The resource-level wait-for relation induced by a set of processes: there is
     a waiting process s holding A and requesting B. *)
  Definition wait_for (procs : list PState) : relation Resource :=
    fun A B => exists s, In s procs /\ In A (held s) /\ B = req s.

  (* KEY REDUCTION (proved, not assumed): every wait-for edge is a lock-order
     edge, directly from `respects`. *)
  Lemma wait_for_subset_lockorder :
    forall procs,
      (forall s, In s procs -> respects s) ->
      inclusion Resource (wait_for procs) R.
  Proof.
    intros procs Hresp A B [s [Hin [Hheld Hreq]]].
    subst B. exact (Hresp s Hin A Hheld).
  Qed.

  Corollary wait_for_subset_Rplus :
    forall procs,
      (forall s, In s procs -> respects s) ->
      inclusion Resource (wait_for procs) Rplus.
  Proof.
    intros procs Hresp A B HW.
    apply R_in_Rplus. exact (wait_for_subset_lockorder procs Hresp A B HW).
  Qed.

  (* Deadlock-freedom for a set of processes = no resource lies on a wait-for
     cycle (the circular-wait Coffman condition is unsatisfiable). *)
  Definition deadlock_free (procs : list PState) : Prop :=
    forall x, ~ clos_trans Resource (wait_for procs) x x.

  (* ===================== MAIN THEOREM ===================== *)
  (* acyclic(R) ⇒ deadlock-free, for ANY finite set of skeleton-respecting
     processes — exactly the states the extraction can produce. *)
  Theorem acyclic_implies_deadlock_free :
    forall procs,
      acyclic ->
      (forall s, In s procs -> respects s) ->
      deadlock_free procs.
  Proof.
    intros procs Hacyc Hresp x Hcyc.
    eapply (cycle_in_acyclic_is_false (wait_for procs) x Hacyc).
    - exact (wait_for_subset_Rplus procs Hresp).
    - exact Hcyc.
  Qed.

  (* Contrapositive — the form the TOOL relies on: a reachable circular-wait
     deadlock implies the lock-order graph has a cycle, so flagging cycles is
     COMPLETE for the modeled deadlocks (and, by the main theorem, acyclicity is
     SOUND for freedom). *)
  Corollary deadlock_implies_lockorder_cycle :
    forall procs,
      (forall s, In s procs -> respects s) ->
      (exists x, clos_trans Resource (wait_for procs) x x) ->
      exists y, Rplus y y.
  Proof.
    intros procs Hresp [x Hcyc].
    exists x.
    exact (clos_trans_mono_into_Rplus (wait_for procs)
             (wait_for_subset_Rplus procs Hresp) x x Hcyc).
  Qed.

End LockOrder.
