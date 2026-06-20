(* ContainmentFunctor — Rocq proof of the strict-sum functor law (ADR-028, item 4).

   The hierarchical rollup (src/hierarchy/rollup.rs) is the action of the
   Containment functor F : Containment -> (Nat, +) on an EXTENSIVE metric
   (a count, e.g. file_count). The functor must preserve composition. For an
   extensive metric this means the workspace total computed by rolling up
   (sum of per-project totals, each the sum of its modules) equals the workspace
   total computed directly (sum over all modules). `categorical_lint` checks this
   at runtime as a data-integrity invariant; here we prove it holds for all
   finite hierarchies.

   Model: a workspace is a list of projects; a project is a list of module
   counts (nat). Builds with coqc with no axioms, admits, or assumptions. *)

From Stdlib Require Import List.
From Stdlib Require Import PeanoNat.
From Stdlib Require Import Lia.
Import ListNotations.

(* The extensive metric folded over a list of leaf counts. *)
Definition sum_list (l : list nat) : nat := fold_right Nat.add 0 l.

(* A project's total = the sum of its module counts (F on the file->module->project leg). *)
Definition project_total (p : list nat) : nat := sum_list p.

(* Workspace total VIA the rollup: sum of per-project totals (the two-step functor image). *)
Definition workspace_via_rollup (w : list (list nat)) : nat :=
  sum_list (map project_total w).

(* Workspace total DIRECTLY: sum over every module, ignoring the grouping. *)
Definition workspace_direct (w : list (list nat)) : nat :=
  sum_list (concat w).

(* Lemma: the extensive metric is additive over concatenation (the monoid
   homomorphism property of sum_list : (list nat, ++) -> (nat, +)). *)
Lemma sum_list_app : forall a b : list nat,
  sum_list (a ++ b) = sum_list a + sum_list b.
Proof.
  induction a as [| x xs IH]; intros b; simpl.
  - reflexivity.
  - rewrite IH. lia.
Qed.

(* Main theorem: the Containment functor preserves the extensive sum — rolling
   up bottom-up equals aggregating directly. This is the STRICT rollup law that
   categorical_lint asserts (a runtime violation is therefore a data-integrity
   bug, never a modeling artifact). *)
Theorem containment_functor_preserves_extensive_sum :
  forall w : list (list nat),
    workspace_via_rollup w = workspace_direct w.
Proof.
  intros w.
  unfold workspace_via_rollup, workspace_direct, project_total.
  induction w as [| p ps IH]; simpl.
  - reflexivity.
  - rewrite sum_list_app. rewrite IH. reflexivity.
Qed.

(* Corollary: associativity of the rollup — inserting an intermediate grouping
   level (the `group` level between project and workspace) does not change the
   extensive total. Grouping the projects into `gs` and summing per group equals
   the flat workspace total. *)
Corollary rollup_level_insertion_is_invariant :
  forall gs : list (list (list nat)),
    sum_list (map workspace_via_rollup gs) = workspace_direct (concat gs).
Proof.
  intros gs.
  unfold workspace_direct.
  induction gs as [| g rest IH]; simpl.
  - reflexivity.
  - rewrite concat_app, sum_list_app.
    rewrite containment_functor_preserves_extensive_sum.
    unfold workspace_direct. rewrite IH. reflexivity.
Qed.
