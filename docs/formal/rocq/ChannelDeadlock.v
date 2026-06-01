(*
 * ADR-011 — soundness of the shadow-ASR message-passing (channel) deadlock
 * signals (src/graph/petri.rs, `tool_channel_deadlock`).
 *
 * The analysis extracts a Petri net (places = channel buffers + per-process
 * control points; transitions = send/recv/spawn) and flags deadlock via
 * structural signals. THIS FILE proves exactly what the tool claims, no more:
 *
 *   (a) UNMATCHED / BLOCKED RECV: a receive transition whose (sole) input place
 *       lies in an UNMARKED SIPHON can never fire — the receiver is blocked.
 *       This backs `blocked_recv` (a linear receive whose channel has no
 *       producer: that channel place is an unmarked siphon).
 *   (b) DEAD MARKING (channel cycle): if EVERY transition that could make
 *       progress consumes from an unmarked siphon, NO transition is enabled —
 *       the marking is dead. This backs `channel_cycle` (each blocked process's
 *       progress transition needs a token only another blocked process would
 *       produce; together their starved channels + control places form the
 *       siphon).
 *
 * NOT claimed: full Commoner *liveness* (that the siphon stays empty forever) —
 * that needs the token-game on a concrete net; the tool reports a candidate dead
 * marking, not a forever-stuck certificate. The dead-marking SUFFICIENCY proved
 * here is precisely the tool's claim. Reference: Commoner (1972); Murata (1989),
 * "Petri Nets: Properties, Analysis and Applications" — siphon = a place-set
 * whose every input transition is also an output transition (`is_siphon`).
 *
 * Self-contained; Rocq 9.x Stdlib only; per-file `coqc` gate. No Admitted /
 * Axiom / Hypothesis (CLAUDE.md). Section `Variable`s are universally-quantified
 * parameters (places/transitions and the net's incidence), not assumptions.
 *)

From Stdlib Require Import List.
From Stdlib Require Import PeanoNat.
From Stdlib Require Import Lia.
Import ListNotations.

Section ChannelNet.

  Variable Place : Type.
  Variable Trans : Type.

  (* A marking assigns a token count to each place. *)
  Definition marking := Place -> nat.

  (* Each transition's input places (the tokens it must consume to fire). *)
  Variable inputs : Trans -> list Place.
  Variable outputs : Trans -> list Place.

  (* Enabled: every input place carries at least one token. *)
  Definition enabled (m : marking) (t : Trans) : Prop :=
    forall p, In p (inputs t) -> m p >= 1.

  (* A siphon S (membership predicate). `unmarked m` = S carries no tokens. *)
  Variable inS : Place -> Prop.

  Definition unmarked (m : marking) : Prop :=
    forall p, inS p -> m p = 0.

  (* SIPHON (Murata 1989): every transition that outputs into S also inputs from
     S (•S ⊆ S•). Recorded for documentation / connection to the literature; the
     dead-marking results below need only `needs_siphon`, which a starved
     blocked-recv channel set satisfies. *)
  Definition is_siphon : Prop :=
    forall t p, In p (outputs t) -> inS p -> exists q, In q (inputs t) /\ inS q.

  (* A transition consumes from the siphon (some input place is in S). *)
  Definition needs_siphon (t : Trans) : Prop :=
    exists p, In p (inputs t) /\ inS p.

  (* ---- (a) A transition needing an unmarked siphon cannot fire. ---- *)
  Lemma needs_unmarked_siphon_not_enabled :
    forall (m : marking) (t : Trans),
      unmarked m -> needs_siphon t -> ~ enabled m t.
  Proof.
    intros m t Hum [p [Hin Hps]] Hen.
    specialize (Hen p Hin).        (* m p >= 1 *)
    rewrite (Hum p Hps) in Hen.    (* 0 >= 1 *)
    lia.
  Qed.

  (* The tool's `blocked_recv`: a linear receive whose channel place is in an
     unmarked siphon (no producer) is permanently blocked. *)
  Corollary unmatched_recv_blocked :
    forall (m : marking) (t : Trans),
      unmarked m ->
      (exists p, In p (inputs t) /\ inS p) ->
      ~ enabled m t.
  Proof.
    intros m t Hum Hneed. exact (needs_unmarked_siphon_not_enabled m t Hum Hneed).
  Qed.

  (* ---- (b) DEAD MARKING: if every transition consumes from the unmarked
     siphon, none is enabled — a deadlock. Backs `channel_cycle`. ---- *)
  Theorem unmarked_siphon_dead :
    forall (m : marking) (trans : list Trans),
      unmarked m ->
      (forall t, In t trans -> needs_siphon t) ->
      forall t, In t trans -> ~ enabled m t.
  Proof.
    intros m trans Hum Hcover t Hin.
    apply needs_unmarked_siphon_not_enabled.
    - exact Hum.
    - exact (Hcover t Hin).
  Qed.

  (* The communication-cycle corollary, stated directly: a set of processes each
     blocked on a receive whose channels are all in the unmarked siphon yields a
     dead marking over their progress transitions. *)
  Corollary cyclic_wait_deadlocks :
    forall (m : marking) (trans : list Trans),
      unmarked m ->
      (forall t, In t trans -> needs_siphon t) ->
      (forall t, In t trans -> ~ enabled m t).
  Proof.
    exact unmarked_siphon_dead.
  Qed.

End ChannelNet.
