(* ======================================================================= *)
(* CsmDeadlockFreedom.v — proofs-as-plans: a well-formed MPST GlobalType is   *)
(* deadlock-free and has progress (Task #22 §4-D).                            *)
(*                                                                            *)
(* This is the machine-checked certificate behind the pgmcp `protocol_sound-  *)
(* ness` MCP tool: rather than model-check NoStuck/Liveness per protocol      *)
(* (pi + TLC), the tool checks MPST well-formedness and cites this lemma —     *)
(* the plan is correct *by construction* (session-types-as-linear-logic:      *)
(* Caires–Pfenning 2010, Wadler 2014).                                        *)
(*                                                                            *)
(* The verify.sh gate runs `coqc` per .v standalone (no inter-file Require),  *)
(* so the minimal `gtype`/`wf`/`gstep` fragment + the progress/preservation   *)
(* theorems are re-stated here byte-faithfully to CsmMpst.v (roles modelled   *)
(* as `nat`).  No Axiom / Hypothesis / Admitted (CLAUDE.md).                   *)
(* ======================================================================= *)

From Stdlib Require Import List Bool Arith.
Import ListNotations.

(* ----------------------------------------------------------------------- *)
(* Minimal MPST global-type fragment (faithful to CsmMpst.v).               *)
(* ----------------------------------------------------------------------- *)

Definition role := nat.
Definition label := nat.

Inductive gtype : Type :=
| GEnd : gtype
| GMsg : role -> role -> label -> gtype -> gtype
| GChoice : role -> role -> list (label * gtype) -> gtype.

(* not-equal-role test as a bool. *)
Definition rne (p q : role) : bool := negb (Nat.eqb p q).

Fixpoint wf (g : gtype) : bool :=
  match g with
  | GEnd => true
  | GMsg p q _ k => rne p q && wf k
  | GChoice p q brs =>
      rne p q
      && negb (Nat.eqb (length brs) 0)
      && forallb (fun pr => wf (snd pr)) brs
  end.

(* One global reduction step: a message exposes its continuation; a choice
   selects one of its branches. *)
Inductive gstep : gtype -> gtype -> Prop :=
| GS_Msg : forall p q l k, gstep (GMsg p q l k) k
| GS_Choice : forall p q brs l k, In (l, k) brs -> gstep (GChoice p q brs) k.

(* ----------------------------------------------------------------------- *)
(* Progress (T3) and subject reduction (T4) — re-proved from the fragment.  *)
(* ----------------------------------------------------------------------- *)

Theorem global_progress :
  forall g, wf g = true -> g <> GEnd -> exists g', gstep g g'.
Proof.
  intros g Hwf Hne.
  destruct g as [| p q l k | p q brs].
  - exfalso. apply Hne. reflexivity.
  - exists k. apply GS_Msg.
  - destruct brs as [| [lab k] rest].
    + simpl in Hwf. rewrite Bool.andb_false_r in Hwf. discriminate.
    + exists k. apply GS_Choice with (l := lab). simpl. left. reflexivity.
Qed.

Theorem wf_preserved :
  forall g g', wf g = true -> gstep g g' -> wf g' = true.
Proof.
  intros g g' Hwf Hstep.
  destruct Hstep as [p q l k | p q brs l k Hin].
  - simpl in Hwf. apply andb_true_iff in Hwf. destruct Hwf as [_ Hk]. exact Hk.
  - simpl in Hwf. apply andb_true_iff in Hwf. destruct Hwf as [_ Hall].
    rewrite forallb_forall in Hall.
    specialize (Hall (l, k) Hin). simpl in Hall. exact Hall.
Qed.

(* ----------------------------------------------------------------------- *)
(* Reachability + deadlock-freedom.                                         *)
(* ----------------------------------------------------------------------- *)

(* Reflexive-transitive closure of gstep: the reachable global states. *)
Inductive gstar : gtype -> gtype -> Prop :=
| GStar_refl : forall g, gstar g g
| GStar_step : forall g g' g'', gstep g g' -> gstar g' g'' -> gstar g g''.

(* A state is stuck iff it is not End and cannot take a step. *)
Definition stuck (g : gtype) : Prop :=
  g <> GEnd /\ ~ (exists g', gstep g g').

(* Well-formedness is preserved along any reachable trace (subject reduction
   lifted to the reflexive-transitive closure). *)
Lemma wf_preserved_star :
  forall g g', gstar g g' -> wf g = true -> wf g' = true.
Proof.
  intros g g' Hstar.
  induction Hstar as [g | g g' g'' Hstep Hstar' IH]; intros Hwf.
  - exact Hwf.
  - apply IH. eapply wf_preserved; eauto.
Qed.

(* PROGRESS: a well-formed global type is either finished or can step. *)
Theorem well_formed_has_progress :
  forall g, wf g = true -> g = GEnd \/ exists g', gstep g g'.
Proof.
  intros g Hwf.
  destruct g as [| p q l k | p q brs].
  - left. reflexivity.
  - right. apply global_progress; [ exact Hwf | discriminate ].
  - right. apply global_progress; [ exact Hwf | discriminate ].
Qed.

(* MAIN — DEADLOCK FREEDOM: no state reachable from a well-formed global type
   is stuck.  This is the NoStuck obligation, discharged once *by typing* for
   all well-formed protocols, demoting the per-protocol model-check to a
   redundant certificate. *)
Theorem well_formed_deadlock_free :
  forall g g', wf g = true -> gstar g g' -> ~ stuck g'.
Proof.
  intros g g' Hwf Hstar.
  assert (wf g' = true) as Hwf' by (eapply wf_preserved_star; eauto).
  intros [Hne Hnostep].
  destruct (well_formed_has_progress g' Hwf') as [Hend | Hstep].
  - apply Hne. exact Hend.
  - apply Hnostep. exact Hstep.
Qed.

(* Corollary keyed to the tool's verdict fields: a well-formed protocol both
   has progress at the root and is deadlock-free throughout its execution. *)
Theorem protocol_soundness_certificate :
  forall g,
    wf g = true ->
    (g = GEnd \/ exists g', gstep g g')                 (* has_progress  *)
    /\ (forall g', gstar g g' -> ~ stuck g').           (* deadlock_free *)
Proof.
  intros g Hwf. split.
  - apply well_formed_has_progress. exact Hwf.
  - intros g' Hstar. eapply well_formed_deadlock_free; eauto.
Qed.
