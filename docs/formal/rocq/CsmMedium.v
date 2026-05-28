(*
 * ADR-009 Phase R1 — the medium discipline, mechanized. RecursiveMAS rides on
 * the same protocol skeleton; only the channel medium (Text vs Latent) differs.
 * A black-box role (no hidden-state access: Claude Code, Codex) may communicate
 * only on Text edges. This file models communications tagged Text|Latent plus a
 * black-box predicate, and proves: a well-media-formed protocol never places a
 * black-box role on a latent edge — the static guarantee `csm::media`'s
 * `check_media_discipline` enforces in Rust.
 *
 * Self-contained (verify.sh runs `coqc` per file); no Admitted/Axiom/Hypothesis.
 *)

From Stdlib Require Import List.
From Stdlib Require Import Bool.

Import ListNotations.

Section Media.

  Variable role : Type.

  Inductive medium := Text | Latent.

  Record comm := { cfrom : role; cto : role; cmed : medium }.

  (* `bb r = true` ⇔ r is a black-box role. *)
  Variable bb : role -> bool.

  Definition involves_black (c : comm) : bool := bb (cfrom c) || bb (cto c).

  Definition is_latent (c : comm) : bool :=
    match cmed c with
    | Latent => true
    | Text => false
    end.

  (* Well-media-formed: no communication is BOTH black-box-involving AND latent. *)
  Definition well_media_formed (cs : list comm) : bool :=
    forallb (fun c => negb (involves_black c && is_latent c)) cs.

  (* The discipline: in a well-media-formed protocol, every latent communication
     involves only white-box roles. *)
  Theorem medium_discipline :
    forall cs c,
      well_media_formed cs = true ->
      In c cs ->
      is_latent c = true ->
      involves_black c = false.
  Proof.
    intros cs c Hwmf Hin Hlat.
    unfold well_media_formed in Hwmf.
    rewrite forallb_forall in Hwmf.
    specialize (Hwmf c Hin).
    rewrite Hlat in Hwmf.
    rewrite andb_true_r in Hwmf.
    apply negb_true_iff in Hwmf.
    exact Hwmf.
  Qed.

  (* Contrapositive corollary: a black-box-involving communication in a
     well-media-formed protocol is necessarily Text. *)
  Corollary black_box_comm_is_text :
    forall cs c,
      well_media_formed cs = true ->
      In c cs ->
      involves_black c = true ->
      is_latent c = false.
  Proof.
    intros cs c Hwmf Hin Hblack.
    destruct (is_latent c) eqn:Hlat.
    - (* latent ⇒ not black-box, contradicting Hblack *)
      assert (Hwhite : involves_black c = false) by
        (apply (medium_discipline cs c Hwmf Hin Hlat)).
      rewrite Hwhite in Hblack. discriminate.
    - reflexivity.
  Qed.

End Media.
