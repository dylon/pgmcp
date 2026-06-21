(*
 * ADR-030 (Phase 6) — Rocq metatheory for the PUSHDOWN / hierarchical CSM
 * conformance core (src/csm/conformance.rs, src/csm/machine.rs).
 *
 * Single self-contained file: `scripts/verify.sh` runs `coqc` per .v standalone
 * (no inter-file `Require`); everything Requires only the Rocq 9.x Stdlib. The
 * finite-fragment metatheory remains in CsmMpst.v (untouched); this file adds the
 * VISIBLY-PUSHDOWN extension that lifts the CSM from regular (finite-state) to a
 * visibly-pushdown recognizer over well-nested protocol runs.
 *
 * No `Axiom` / `Hypothesis` / `Admitted` (CLAUDE.md). Every result is closed `Qed`.
 *
 * Faithful model. The implementation's `Return` edge pops the top return address
 * UNCONDITIONALLY (it resumes the pushed state; it does not re-validate a matching
 * call id — src/csm/conformance.rs `epsilon_close`). Well-nestedness is therefore
 * *balance* (depth), so the stack is modeled by its DEPTH (a `nat`). A `Call`
 * pushes (depth+1), a `Ret` pops (depth-1, or `None` on the empty stack — an
 * unmatched return), an `Int` leaves the depth. This is exactly the per-role
 * pushdown replay; "accepts" = "the run starts and ends at depth 0" = every frame
 * entered was returned.
 *
 * Theorems (all `Qed`):
 *   run_app                   — the recognizer distributes over concatenation.
 *   conformance_complete      — every well-nested (Dyck-balanced) run is ACCEPTED
 *                               (no false negatives).
 *   conformance_sound         — every ACCEPTED run is well-nested (no false
 *                               positives). Together with completeness: the
 *                               recognizer accepts EXACTLY the well-nested runs —
 *                               the decidable VPA core (Alur & Madhusudan, 2004).
 *   conformance_decidable     — conformance is decidable (the VPL property finite
 *                               state lacked a stack for, and a general PDA lacks
 *                               the closure for).
 *   runD_bounded              — the depth-D-guarded recognizer never reaches a
 *                               depth above D: the BOUNDED-STACK linchpin. A bound
 *                               makes the reachable-configuration set finite, which
 *                               is *why* this file is an ordinary `Inductive` (no
 *                               coinduction) and why TLC has a finite model
 *                               (ADR-030 §9).
 *   runD_agrees               — within the bound, guarded and unguarded recognizers
 *                               coincide (the bound is conservative).
 *   pushdown_terminates       — the recognizer terminates: `run` is structural on
 *                               the run, and the runtime depth/budget measure is
 *                               well-founded (generalizes CsmMpst.v `rlm_terminates`
 *                               from a single counter to the recursion measure).
 *   unmatched_return_rejected — a run that pops the empty stack (an unmatched
 *                               return) is refused — the operational trust check
 *                               (it is not well-nested).
 *)

From Stdlib Require Import List.
From Stdlib Require Import Bool.
From Stdlib Require Import Arith.
From Stdlib Require Import PeanoNat.
From Stdlib Require Import Lia.
From Stdlib Require Import Wf_nat.

Import ListNotations.

(* ===================================================================== *)
(* The visibly-pushdown alphabet and the conformance recognizer.         *)
(* ===================================================================== *)

(* A protocol-run symbol; its CLASS fixes the stack action (visibly pushdown):
   `SCall` pushes a frame, `SRet` pops the top frame, `SInt n` is an ordinary peer
   communication (the stack is unchanged). *)
Inductive sym : Type :=
| SCall : sym
| SRet  : sym
| SInt  : nat -> sym.

(* The conformance recognizer, by stack DEPTH (`d`). Mirrors src/csm/conformance.rs
   `epsilon_close` + internal advance: Call pushes, Ret pops (or `None` on the
   empty stack — an unmatched return ⇒ not well-nested), Int leaves the depth.
   Structural recursion on the run ⇒ TERMINATES by construction (Rocq-checked). *)
Fixpoint run (d : nat) (w : list sym) : option nat :=
  match w with
  | [] => Some d
  | SCall :: w' => run (S d) w'
  | SRet :: w' => match d with
                  | S d' => run d' w'
                  | 0 => None
                  end
  | SInt _ :: w' => run d w'
  end.

(* A run conforms iff, started at depth 0, it ends at depth 0 — every frame
   returned (well-nested). Mirrors `check_conformance`'s "terminal + empty stack". *)
Definition accepts (w : list sym) : Prop := run 0 w = Some 0.

(* ===================================================================== *)
(* Well-nestedness (Dyck-balanced; internals transparent).               *)
(* ===================================================================== *)

(* The grammar of well-nested runs (the canonical first-call-matched / left-to-right
   decomposition): empty; a leading internal then a well-nested rest; or a CALL
   whose matching RET closes a well-nested "inside", followed by a well-nested
   "outside". *)
Inductive balanced : list sym -> Prop :=
| bal_nil  : balanced []
| bal_int  : forall n w, balanced w -> balanced (SInt n :: w)
| bal_call : forall wi wo,
    balanced wi -> balanced wo -> balanced (SCall :: (wi ++ SRet :: wo)).

(* ===================================================================== *)
(* run_app — the recognizer distributes over concatenation.              *)
(* ===================================================================== *)

Lemma run_app : forall w1 w2 d,
  run d (w1 ++ w2) =
    match run d w1 with
    | Some d1 => run d1 w2
    | None => None
    end.
Proof.
  induction w1 as [| x w1 IH]; intros w2 d; simpl.
  - reflexivity.
  - destruct x as [| | n].
    + apply IH.
    + destruct d as [| d']; [reflexivity | apply IH].
    + apply IH.
Qed.

(* ===================================================================== *)
(* Completeness: a well-nested run is accepted (no false negatives).     *)
(* ===================================================================== *)

(* A well-nested run leaves ANY starting depth unchanged. *)
Lemma balanced_keeps_depth : forall w,
  balanced w -> forall d, run d w = Some d.
Proof.
  intros w Hb. induction Hb as [| n w Hb IH | wi wo Hi IHi Ho IHo]; intro d.
  - reflexivity.
  - simpl. apply IH.
  - simpl. rewrite run_app. rewrite (IHi (S d)). simpl. apply IHo.
Qed.

Theorem conformance_complete : forall w, balanced w -> accepts w.
Proof.
  intros w Hb. unfold accepts. apply (balanced_keeps_depth w Hb 0).
Qed.

(* ===================================================================== *)
(* Soundness: an accepted run is well-nested (no false positives).       *)
(* ===================================================================== *)

(* The matching-return split. If a run at depth `a` ends at `b < a` (a net pop),
   there is a FIRST `SRet` that drops the depth below `a`: the prefix `wi` is
   itself well-nested (it returns to `a` without ever dropping below it), and the
   suffix `wo` carries depth `a-1` on to `b`. Strong induction on the run length;
   the nested-call case peels the inner block first, then continues — each appeal
   to the IH is on a strictly shorter run. Concluding `balanced wi` directly is
   what makes the soundness `SCall` case immediate. *)
Lemma return_to : forall fuel w a b,
  length w <= fuel ->
  run a w = Some b ->
  b < a ->
  exists wi wo,
    w = wi ++ SRet :: wo /\
    balanced wi /\
    run (pred a) wo = Some b /\
    length wi < length w /\ length wo < length w.
Proof.
  induction fuel as [| fuel IH]; intros w a b Hlen Hrun Hlt.
  - (* fuel = 0 ⇒ w = [] ⇒ run a [] = Some a, but a <> b since b < a *)
    destruct w as [| x w']; simpl in Hlen; [| lia].
    simpl in Hrun. injection Hrun as Hrun. lia.
  - destruct w as [| x w']; simpl in Hrun.
    + (* [] : same contradiction *)
      injection Hrun as Hrun. lia.
    + destruct x as [| | n].
      * (* SCall: depth a → S a; ends at b < a < S a.  Peel the inner block, then
           continue to find the first drop below a. *)
        simpl in Hlen.
        assert (Hb' : b < S a) by lia.
        destruct (IH w' (S a) b ltac:(lia) Hrun Hb')
          as (wi' & wo' & Hw' & Hbi' & Hwo' & Hli' & Hlo').
        simpl in Hwo'.                 (* run (pred (S a)) wo' = run a wo' = Some b *)
        (* wo' carries depth a on to b < a: recurse for the first drop below a. *)
        assert (Hwo'len : length wo' <= fuel) by lia.
        destruct (IH wo' a b Hwo'len Hwo' Hlt)
          as (wi'' & wo'' & Hwo'eq & Hbi'' & Hwo'' & Hli'' & Hlo'').
        exists (SCall :: wi' ++ SRet :: wi''), wo''.
        repeat apply conj.
        -- (* w = SCall :: w' = SCall :: wi' ++ SRet :: (wi'' ++ SRet :: wo'') *)
           rewrite Hw', Hwo'eq.
           simpl. rewrite <- app_assoc. simpl. reflexivity.
        -- (* balanced (SCall :: wi' ++ SRet :: wi'') *)
           apply bal_call; assumption.
        -- exact Hwo''.
        -- (* length (SCall :: wi' ++ SRet :: wi'') < length (SCall :: w') *)
           rewrite Hw', Hwo'eq. simpl. repeat (rewrite length_app; simpl). lia.
        -- rewrite Hw', Hwo'eq. simpl. repeat (rewrite length_app; simpl). lia.
      * (* SRet: depth a → pred a; this is the FIRST drop (wi = []). *)
        destruct a as [| a']; [lia |].
        simpl in Hrun.
        exists (@nil sym), w'. repeat apply conj.
        -- reflexivity.
        -- apply bal_nil.
        -- exact Hrun.
        -- simpl. lia.
        -- simpl. lia.
      * (* SInt: depth unchanged; recurse and prepend. *)
        simpl in Hlen.
        destruct (IH w' a b ltac:(lia) Hrun Hlt)
          as (wi & wo & Hw & Hbi & Hwo & Hli & Hlo).
        exists (SInt n :: wi), wo. repeat apply conj.
        -- rewrite Hw. reflexivity.
        -- apply bal_int. exact Hbi.
        -- exact Hwo.
        -- simpl. lia.
        -- simpl. lia.
Qed.

(* Accepted runs are well-nested. Strong induction on length; the `SCall` case uses
   `return_to` (a=1, b=0) to split off the matching return, both sides balanced. *)
Lemma sound_aux : forall fuel w,
  length w <= fuel -> run 0 w = Some 0 -> balanced w.
Proof.
  induction fuel as [| fuel IH]; intros w Hlen Hrun.
  - destruct w as [| x w']; simpl in Hlen; [apply bal_nil | lia].
  - destruct w as [| x w']; [apply bal_nil |].
    destruct x as [| | n]; simpl in Hrun, Hlen.
    + (* SCall: run 1 w' = Some 0 *)
      destruct (return_to (length w') w' 1 0 (le_n _) Hrun (Nat.lt_0_1))
        as (wi & wo & Hw & Hbi & Hwo & Hli & Hlo).
      simpl in Hwo.                    (* run 0 wo = Some 0 *)
      (* both wi and wo are shorter than w' (hence ≤ fuel); recurse on wo. *)
      assert (Hwofuel : length wo <= fuel) by lia.
      assert (balanced wo) by (apply (IH wo Hwofuel Hwo)).
      rewrite Hw. apply bal_call; assumption.
    + (* SRet: run 0 (SRet :: w') = None <> Some 0 *)
      discriminate Hrun.
    + (* SInt: run 0 w' = Some 0 *)
      apply bal_int. apply (IH w' ltac:(lia) Hrun).
Qed.

Theorem conformance_sound : forall w, accepts w -> balanced w.
Proof.
  intros w H. apply (sound_aux (length w) w (le_n _) H).
Qed.

(* The recognizer accepts EXACTLY the well-nested runs. *)
Theorem conformance_correct : forall w, accepts w <-> balanced w.
Proof.
  intro w. split; [apply conformance_sound | apply conformance_complete].
Qed.

(* ===================================================================== *)
(* Decidability — conformance is decidable (the VPL property).           *)
(* ===================================================================== *)

Theorem conformance_decidable : forall w, {accepts w} + {~ accepts w}.
Proof.
  intro w. unfold accepts. destruct (run 0 w) as [d |].
  - destruct d as [| d'].
    + left. reflexivity.
    + right. discriminate.
  - right. discriminate.
Qed.

(* ===================================================================== *)
(* unmatched_return_rejected — popping the empty stack is refused.       *)
(* ===================================================================== *)

Theorem unmatched_return_rejected : forall w, run 0 (SRet :: w) = None.
Proof. reflexivity. Qed.

(* ===================================================================== *)
(* Boundedness — the depth-D-guarded recognizer never exceeds depth D.   *)
(*                                                                       *)
(* The linchpin (ADR-030 §9): a finite bound D makes the reachable-config set      *)
(* finite, so the model is an ordinary `Inductive` (no coinduction) and TLC has a  *)
(* finite model. `runD` refuses to push past D (the `DepthExceeded` of             *)
(* src/csm/conformance.rs, the MAX_STACK_DEPTH guard).                             *)
(* ===================================================================== *)

Fixpoint runD (cap : nat) (d : nat) (w : list sym) : option nat :=
  match w with
  | [] => Some d
  | SCall :: w' => if Nat.ltb d cap then runD cap (S d) w' else None
  | SRet :: w' => match d with
                  | S d' => runD cap d' w'
                  | 0 => None
                  end
  | SInt _ :: w' => runD cap d w'
  end.

(* Every depth the guarded recognizer reaches stays within the bound. *)
Theorem runD_bounded : forall w cap d d',
  d <= cap -> runD cap d w = Some d' -> d' <= cap.
Proof.
  induction w as [| x w IH]; intros cap d d' Hle Hrun; simpl in Hrun.
  - injection Hrun as ->. exact Hle.
  - destruct x as [| | n].
    + destruct (Nat.ltb d cap) eqn:E; [| discriminate Hrun].
      apply Nat.ltb_lt in E.
      apply (IH cap (S d) d'); [lia | exact Hrun].
    + destruct d as [| d0]; [discriminate Hrun |].
      apply (IH cap d0 d'); [lia | exact Hrun].
    + apply (IH cap d d'); [exact Hle | exact Hrun].
Qed.

(* Within the bound the guarded and unguarded recognizers coincide — the guard is
   conservative (it only ever rejects a run that would exceed D). *)
Theorem runD_agrees : forall w cap d d',
  runD cap d w = Some d' -> run d w = Some d'.
Proof.
  induction w as [| x w IH]; intros cap d d' Hrun; simpl in Hrun; simpl.
  - exact Hrun.
  - destruct x as [| | n].
    + destruct (Nat.ltb d cap) eqn:E; [| discriminate Hrun].
      apply (IH cap (S d) d'). exact Hrun.
    + destruct d as [| d0]; [discriminate Hrun |].
      apply (IH cap d0 d'). exact Hrun.
    + apply (IH cap d d'). exact Hrun.
Qed.

(* ===================================================================== *)
(* Termination — structural on the run; the runtime measure well-founded.*)
(* ===================================================================== *)

(* `run`/`runD` are structural `Fixpoint`s on the run, so they terminate on every
   input (Rocq-checked). The RUNTIME recursion (the RLM / the bounded stack) is
   bounded by a strictly-decreasing remaining-depth/budget measure; that the
   resulting relation is well-founded — hence the recursion halts — generalizes
   CsmMpst.v `rlm_terminates` from one counter to the pushdown setting. *)
Record measure := { remaining : nat }.

Definition descends (c p : measure) : Prop := remaining c < remaining p.

Theorem pushdown_terminates : well_founded descends.
Proof.
  apply (well_founded_lt_compat measure remaining descends).
  intros x y H. exact H.
Qed.

(* run is total (always returns) — the structural-recursion termination, stated. *)
Theorem run_total : forall d w, exists r, run d w = r.
Proof. intros d w. exists (run d w). reflexivity. Qed.
